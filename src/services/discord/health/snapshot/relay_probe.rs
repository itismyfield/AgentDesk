//! Operand assembly for the composed relay verdict — #5942. Split out of
//! `super` (`health/snapshot.rs`, a registered giant per
//! `scripts/giant_file_registry.toml`, #5447) with no behavior change: same
//! bodies, `#[cfg(unix)]` moved to the `mod` declaration.
//!
//! The expiry these operands can produce surfaces on
//! `DiscordHealthSnapshot::expired_relay_ledgers`, beside `degraded_reasons`
//! rather than inside it — it must not be silent, but must also not be
//! counted into the degraded axis (that conflation is the saturation #5942
//! reported).
//!
//! An empty vector is NOT the normal steady state; a reader must look at the
//! entry COUNT and `unobserved_for_secs`, never at emptiness. There is no such
//! reader in this repo yet — #5947 tracks adding one or removing the field.

use crate::services::discord::relay_health::{RelayActiveTurn, RelayHealthSnapshot};
use crate::services::provider::ProviderKind;

use super::super::reachability::composite::RowlessTurn;
use super::super::reachability::ledger::{ledger_file_exists, ledger_path};
use super::super::session_enrichment::ExecutorWitness;

pub(super) struct RelayVerdictProbeOperands {
    pub(super) pane_idle_confirmed: bool,
    pub(super) rowless_turn: RowlessTurn,
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
        rowless_turn: if relay_health.unpaired_active_token_outlived_grace() {
            RowlessTurn::OutlivedGrace
        } else if relay_health.unpaired_active_token_reconfirmed {
            RowlessTurn::WithinGrace
        } else {
            RowlessTurn::None
        },
        executor,
        placeholder_present: relay_health.pending_discord_callback_msg_id.is_some(),
        now_epoch_ms: chrono::Utc::now().timestamp_millis().max(0) as u64,
        process_started_at_epoch_ms: process_started_at_unix.max(0).saturating_mul(1_000) as u64,
    }
}

/// The detail path's execution-owner witness (#5942).
///
/// The two health paths intentionally do NOT share a probe — they ask
/// different questions:
///
/// * AGGREGATE (`build_health_snapshot_with_options`) probes
///   `tmux::session_presence` — "does the session exist" — because its
///   derived `tmux_present` bool has consumers across `super` and stall
///   recovery that #5942 does not change;
/// * DETAIL probes `tmux::pane_liveness` — "does the session have a live
///   pane" — matching what `tmux_session_alive` has always published.
///
/// Both share one property the ledger TTL relies on: a probe that could not
/// answer yields [`ExecutorWitness::Unwitnessed`] and therefore cannot expire
/// anything (`PaneLiveness::ProbeError` → `None`; `SessionPresence::ProbeFailed`
/// → `Unwitnessed` in `witness_tmux_session_within`).
///
/// They diverge in three places, pinned by
/// `the_two_health_paths_agree_inside_the_probe_and_diverge_on_the_wedge_the_budget_and_the_blank_name`:
///
/// * dead-pane wedge (session exists, only dead panes) — aggregate reads
///   `Present` (blocks expiry), detail reads `Absent` (would expire);
/// * shared probe budget — only the aggregate path is charged against it, so
///   an exhausted budget withholds only the aggregate witness;
/// * blank session name — `tmux::session_presence` rejects it as
///   `ProbeFailed` → `Unwitnessed`; `tmux::pane_liveness` rejects it as
///   `DeadOrAbsent` → `Some(false)` → `Absent` here.
///
/// The first two err safe: the aggregate path decides `/api/health`'s `ok`
/// and there it refuses to expire. The blank-name divergence is harmless for
/// a different reason — the detail path publishes no expiry, it only
/// abstains from the warrant.
///
/// `(None, None)` is a positive absence, not a probe fault:
/// `probe_tmux_session_alive` returns `None` without probing when there is no
/// session name, matching the aggregate path's rule for the same situation.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::relay_health::{
        RelayStallClassifier, RelayStallState, UNPAIRED_ACTIVE_TOKEN_GRACE_SECS,
    };

    /// The 2026-09-18 adk-dash-cc capture: token held, no inflight row, the
    /// pairing reconfirmed, three messages queued behind it.
    fn rowless_capture(turn_age_secs: u64) -> RelayHealthSnapshot {
        RelayHealthSnapshot {
            active_turn: RelayActiveTurn::Foreground,
            tmux_alive: Some(true),
            watcher_attached: true,
            mailbox_has_cancel_token: true,
            mailbox_active_user_msg_id: Some(1_550_401_623_417_950_271),
            mailbox_turn_started_at_ms: Some(1_000_000),
            mailbox_turn_age_secs: Some(turn_age_secs),
            queue_depth: 3,
            unpaired_active_token_reconfirmed: true,
            ..RelayHealthSnapshot::test_snapshot()
        }
    }

    /// The reachability verdict reports a rowless turn as stuck exactly where
    /// the stall classifier calls it `UnpairedActiveToken`, so a token still
    /// inside its row-acquisition grace no longer pins the node degraded.
    #[test]
    fn a_rowless_turn_is_stuck_only_where_the_stall_classifier_calls_it_unpaired() {
        for (age, expected) in [
            (0, RowlessTurn::WithinGrace),
            (
                UNPAIRED_ACTIVE_TOKEN_GRACE_SECS - 1,
                RowlessTurn::WithinGrace,
            ),
            (UNPAIRED_ACTIVE_TOKEN_GRACE_SECS, RowlessTurn::OutlivedGrace),
            (738, RowlessTurn::OutlivedGrace),
        ] {
            let capture = rowless_capture(age);
            assert_eq!(
                relay_verdict_probe_operands(ExecutorWitness::Present, &capture, 1).rowless_turn,
                expected,
                "turn age {age}s"
            );
            assert_eq!(
                RelayStallClassifier::classify(&capture) == RelayStallState::UnpairedActiveToken,
                expected == RowlessTurn::OutlivedGrace,
                "the stall classifier disagrees at turn age {age}s"
            );
        }

        let unconfirmed = RelayHealthSnapshot {
            unpaired_active_token_reconfirmed: false,
            ..rowless_capture(738)
        };
        assert_eq!(
            relay_verdict_probe_operands(ExecutorWitness::Present, &unconfirmed, 1).rowless_turn,
            RowlessTurn::None,
            "an unreconfirmed pairing is not a rowless turn at any age"
        );
    }
}
