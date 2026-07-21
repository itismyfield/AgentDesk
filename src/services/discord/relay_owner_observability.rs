//! #3646 — relay owner observation (OBSERVATION-ONLY, behaviour unchanged).
//!
//! Background: the relay flight recorder (`tmux_watcher.rs`) read the relay
//! owner ONLY from the pre-relay inflight snapshot (`inflight_before_relay`).
//! Once the bridge cleared inflight but the finalizer ledger still held a
//! `Watcher`/finalized entry, the recorder collapsed both distinct states into
//! a single `relay_owner_kind="none"`. That one ambiguous signal was the direct
//! cause of the #3607 misdiagnosis loop: a real None-ledger turn and a
//! "bridge cleared inflight but ledger is Watcher-finalized" turn looked
//! identical in the logs.
//!
//! This module is PURE and side-effect free. It only builds the JSON payloads
//! and derives the terminal-UI transition outcome / the inflight-clear invariant
//! condition for the three new observation events that `tmux_watcher.rs` and
//! `turn_finalizer.rs` emit through the existing
//! `crate::services::observability::emit_inflight_lifecycle_event` infra. No
//! relay / cleanup branch, ordering, or condition is changed by anything here —
//! callers read values they already hold and pass them in.

use serde_json::{Value, json};

/// Stable `kind` strings for the three #3646 lifecycle observation events plus
/// the finalizer-side ledger-owner companion. Centralised so the PG/jsonl
/// consumers and the unit tests share one source of truth.
pub(in crate::services::discord) mod event_kind {
    pub(in crate::services::discord) const TERMINAL_BODY_COMMIT: &str = "terminal_body_commit";
    pub(in crate::services::discord) const TERMINAL_UI_TRANSITION: &str = "terminal_ui_transition";
    pub(in crate::services::discord) const INFLIGHT_CLEAR: &str = "inflight_clear";
    pub(in crate::services::discord) const FINALIZER_LEDGER_OWNER: &str = "finalizer_ledger_owner";
}

/// Counter name for the #3646 inflight-clear invariant breach (the #3607
/// state: terminal delivery committed, but cleared with neither a visible UI
/// completion nor a persisted terminal-UI obligation). Used by the
/// error-level invariant signal — NOT a control-flow gate.
pub(in crate::services::discord) const INFLIGHT_CLEAR_COMMITTED_NO_UI_FINISH_INVARIANT: &str =
    "inflight_clear_committed_no_ui_finish";

/// The visible-UI-transition outcome the watcher took for a committed terminal
/// turn. Maps onto the issue #3646 `outcome` enum
/// (`committed | gate_suppressed | edit_failed | stale_identity`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum TerminalUiOutcome {
    /// The completion was emitted to the user (gate said `should_emit_completion`).
    Committed,
    /// The TUI quiescence gate suppressed the visible completion (TimedOut).
    GateSuppressed,
    /// The pre-relay snapshot was a stale NEWER turn, so the older committed
    /// range did not touch that newer turn's panel.
    StaleIdentity,
}

impl TerminalUiOutcome {
    pub(in crate::services::discord) fn as_str(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::GateSuppressed => "gate_suppressed",
            Self::StaleIdentity => "stale_identity",
        }
    }

    /// Derive the observed UI transition from the watcher's already-computed
    /// signals. PURE: identical inputs the watcher already branches on, no new
    /// decision is taken — this only LABELS the path the watcher took.
    ///
    /// Precedence mirrors the watcher control flow:
    /// 1. a stale NEWER-turn snapshot short-circuits the visible completion EDIT
    ///    (`#3142` gate) → `StaleIdentity`;
    /// 2. otherwise, the TUI completion gate decides: emit → `Committed`,
    ///    suppress → `GateSuppressed`.
    pub(in crate::services::discord) fn derive(
        is_stale_newer_turn: bool,
        should_emit_completion: bool,
    ) -> Self {
        if is_stale_newer_turn {
            Self::StaleIdentity
        } else if should_emit_completion {
            Self::Committed
        } else {
            Self::GateSuppressed
        }
    }
}

/// Build the `discord:<channel>:<user_msg_id>` turn identity string the #3646
/// events carry so the watcher-side `inflight_relay_owner` and the
/// finalizer-side `ledger_relay_owner` rows JOIN on one key in PG.
pub(in crate::services::discord) fn turn_identity_string(
    channel_id: u64,
    user_msg_id: u64,
) -> String {
    format!("discord:{channel_id}:{user_msg_id}")
}

/// Build the `terminal_body_commit` event payload (#3646 event 1/3). Records
/// BOTH owner signals available at the commit chokepoint: `inflight_relay_owner`
/// from the pre-relay snapshot, and `ledger_relay_owner` which is NOT readable
/// here (the ledger is actor-owned in `turn_finalizer`) so it is `null` and the
/// companion `finalizer_ledger_owner` event supplies it on the same `turn_id`.
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn terminal_body_commit_extra(
    turn_identity: &str,
    user_msg_id: u64,
    finalizer_turn_id: u64,
    status_message_id: u64,
    committed_range_start: u64,
    committed_range_end: u64,
    inflight_relay_owner: &str,
    terminal_delivery_committed: bool,
) -> Value {
    json!({
        "turn_identity": turn_identity,
        "user_msg_id": user_msg_id,
        "finalizer_turn_id": finalizer_turn_id,
        "status_message_id": status_message_id,
        "committed_range": [committed_range_start, committed_range_end],
        "inflight_relay_owner": inflight_relay_owner,
        // Not readable at the watcher commit site (actor-owned ledger); the
        // `finalizer_ledger_owner` event carries it under the same turn_id.
        "ledger_relay_owner": Value::Null,
        "terminal_delivery_committed": terminal_delivery_committed,
    })
}

/// Build the `terminal_ui_transition` event payload (#3646 event 2/3).
pub(in crate::services::discord) fn terminal_ui_transition_extra(
    outcome: TerminalUiOutcome,
    gate_outcome: &str,
    pane_quiescent: Option<bool>,
    obligation_id: Option<u64>,
) -> Value {
    json!({
        "outcome": outcome.as_str(),
        "gate_outcome": gate_outcome,
        "pane_quiescent": pane_quiescent,
        "obligation_id": obligation_id,
    })
}

/// The #3646 inflight-clear invariant: a committed terminal delivery cleared
/// WITHOUT either a visible UI completion or a persisted terminal-UI obligation
/// is the #3607 state. Returns `true` when the invariant is VIOLATED.
///
/// PURE: callers pass values they already hold. This does NOT gate the clear —
/// the clear runs regardless; the result only drives an error-level signal.
pub(in crate::services::discord) fn inflight_clear_invariant_violated(
    terminal_delivery_committed: bool,
    terminal_ui_committed: bool,
    terminal_ui_obligation_persisted: bool,
) -> bool {
    terminal_delivery_committed && !(terminal_ui_committed || terminal_ui_obligation_persisted)
}

/// Build the `inflight_clear` event payload (#3646 event 3/3).
pub(in crate::services::discord) fn inflight_clear_extra(
    terminal_delivery_committed: bool,
    terminal_ui_committed: bool,
    terminal_ui_obligation_persisted: bool,
    invariant_violated: bool,
) -> Value {
    json!({
        "terminal_delivery_committed": terminal_delivery_committed,
        "terminal_ui_committed": terminal_ui_committed,
        "terminal_ui_obligation_persisted": terminal_ui_obligation_persisted,
        "invariant_violated": invariant_violated,
    })
}

/// Build the `finalizer_ledger_owner` companion payload (emitted from
/// `turn_finalizer` where the actor-owned ledger entry's `relay_owner` IS
/// readable). JOINs to the watcher-side `terminal_body_commit` on `turn_id`.
pub(in crate::services::discord) fn finalizer_ledger_owner_extra(
    ledger_relay_owner: &str,
    terminal_event: &str,
    clear_inflight: bool,
) -> Value {
    json!({
        "ledger_relay_owner": ledger_relay_owner,
        "terminal_event": terminal_event,
        "clear_inflight": clear_inflight,
    })
}

// ----------------------------------------------------------------------------
// Emit wrappers. These own the orchestration (turn-id derivation + the
// `emit_inflight_lifecycle_event` call) so the #3016 hot file
// (`tmux_watcher.rs`) and `turn_finalizer.rs` only pass the signals they
// already hold. OBSERVATION-ONLY: each is a single fire-and-forget emit, no
// control flow.
// ----------------------------------------------------------------------------

/// Emit #3646 event 1/3 (`terminal_body_commit`). `user_msg_id == 0`
/// (external/injected/TUI-direct) carries no correlation turn_id but still emits
/// the `turn_identity` field for JOINs.
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn emit_terminal_body_commit(
    provider: &str,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    finalizer_turn_id: u64,
    user_msg_id: u64,
    status_message_id: u64,
    committed_range_start: u64,
    committed_range_end: u64,
    inflight_relay_owner: &str,
    terminal_delivery_committed: bool,
) {
    let turn_identity = turn_identity_string(channel_id, user_msg_id);
    let turn_id = (user_msg_id != 0).then(|| turn_identity.clone());
    crate::services::observability::emit_inflight_lifecycle_event(
        provider,
        channel_id,
        dispatch_id,
        session_key,
        turn_id.as_deref(),
        event_kind::TERMINAL_BODY_COMMIT,
        terminal_body_commit_extra(
            &turn_identity,
            user_msg_id,
            finalizer_turn_id,
            status_message_id,
            committed_range_start,
            committed_range_end,
            inflight_relay_owner,
            terminal_delivery_committed,
        ),
    );
}

/// Emit #3646 event 2/3 (`terminal_ui_transition`). `obligation_id` is always
/// `None` from the watcher path (the watcher does not persist obligations).
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn emit_terminal_ui_transition(
    provider: &str,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    user_msg_id: u64,
    outcome: TerminalUiOutcome,
    gate_outcome: &str,
    pane_quiescent: Option<bool>,
) {
    let turn_id = (user_msg_id != 0).then(|| turn_identity_string(channel_id, user_msg_id));
    crate::services::observability::emit_inflight_lifecycle_event(
        provider,
        channel_id,
        dispatch_id,
        session_key,
        turn_id.as_deref(),
        event_kind::TERMINAL_UI_TRANSITION,
        terminal_ui_transition_extra(outcome, gate_outcome, pane_quiescent, None),
    );
}

/// Emit #3646 event 3/3 (`inflight_clear`) plus the non-fatal ERROR-level
/// invariant signal for the #3607 state (committed cleared without UI finish).
/// The clear itself has ALREADY run at the call site — this records the signals
/// and fires the invariant; it NEVER gates cleanup. `record_invariant_check_*`
/// only tracing-logs + emits + bumps `guard_fires`; its bool is discarded.
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn emit_inflight_clear_with_invariant(
    provider: &str,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    terminal_delivery_committed: bool,
    terminal_ui_committed: bool,
    terminal_ui_obligation_persisted: bool,
) {
    let invariant_violated = inflight_clear_invariant_violated(
        terminal_delivery_committed,
        terminal_ui_committed,
        terminal_ui_obligation_persisted,
    );
    crate::services::observability::emit_inflight_lifecycle_event(
        provider,
        channel_id,
        dispatch_id,
        session_key,
        turn_id,
        event_kind::INFLIGHT_CLEAR,
        inflight_clear_extra(
            terminal_delivery_committed,
            terminal_ui_committed,
            terminal_ui_obligation_persisted,
            invariant_violated,
        ),
    );
    // #3646: NON-FATAL invariant signal — NO panic/assert! on the operational
    // relay/cleanup path (turn-wedge / crash risk). `condition = NOT violated`.
    let _ = crate::services::observability::record_invariant_check_with_severity(
        !invariant_violated,
        crate::services::observability::InvariantViolation {
            provider: Some(provider),
            channel_id: Some(channel_id),
            dispatch_id,
            session_key,
            turn_id,
            invariant: INFLIGHT_CLEAR_COMMITTED_NO_UI_FINISH_INVARIANT,
            code_location: "tmux_watcher.rs:inflight_clear",
            message: "committed terminal delivery cleared without visible UI completion or persisted terminal-UI obligation (#3607)",
            details: inflight_clear_extra(
                terminal_delivery_committed,
                terminal_ui_committed,
                terminal_ui_obligation_persisted,
                invariant_violated,
            ),
        },
        crate::services::observability::InvariantSeverity::Error,
    );
    // Dev-only secondary guard: surfaces the #3607 state loudly in debug/test
    // builds; compiles OUT of release/operational binaries so it can never wedge
    // a turn or crash the relay path in production.
    debug_assert!(
        !invariant_violated,
        "#3646/#3607: committed terminal cleared without UI completion or persisted obligation"
    );
}

/// Emit the #3646 `finalizer_ledger_owner` companion from the finalizer actor
/// task, where the ledger entry's `relay_owner` is readable. JOINs to the
/// watcher `terminal_body_commit` on the same turn_id.
pub(in crate::services::discord) fn emit_finalizer_ledger_owner(
    provider: &str,
    channel_id: u64,
    user_msg_id: u64,
    ledger_relay_owner: &str,
    terminal_event: &str,
    clear_inflight: bool,
) {
    let turn_id = (user_msg_id != 0).then(|| turn_identity_string(channel_id, user_msg_id));
    crate::services::observability::emit_inflight_lifecycle_event(
        provider,
        channel_id,
        None,
        None,
        turn_id.as_deref(),
        event_kind::FINALIZER_LEDGER_OWNER,
        finalizer_ledger_owner_extra(ledger_relay_owner, terminal_event, clear_inflight),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_outcome_stale_identity_takes_precedence_over_gate() {
        // A stale NEWER-turn snapshot short-circuits the visible completion EDIT
        // regardless of the gate decision (#3142 gate before the gate emit).
        assert_eq!(
            TerminalUiOutcome::derive(true, true),
            TerminalUiOutcome::StaleIdentity
        );
        assert_eq!(
            TerminalUiOutcome::derive(true, false),
            TerminalUiOutcome::StaleIdentity
        );
    }

    #[test]
    fn ui_outcome_committed_when_gate_emits_and_not_stale() {
        assert_eq!(
            TerminalUiOutcome::derive(false, true),
            TerminalUiOutcome::Committed
        );
        assert_eq!(TerminalUiOutcome::Committed.as_str(), "committed");
    }

    #[test]
    fn ui_outcome_gate_suppressed_when_gate_does_not_emit() {
        assert_eq!(
            TerminalUiOutcome::derive(false, false),
            TerminalUiOutcome::GateSuppressed
        );
        assert_eq!(
            TerminalUiOutcome::GateSuppressed.as_str(),
            "gate_suppressed"
        );
    }

    #[test]
    fn invariant_violated_only_on_committed_without_ui_or_obligation() {
        // #3607 state: committed, but neither UI completion nor obligation.
        assert!(inflight_clear_invariant_violated(true, false, false));
        // A visible UI completion satisfies the invariant.
        assert!(!inflight_clear_invariant_violated(true, true, false));
        // A persisted obligation satisfies the invariant (the bridge gate-timeout path).
        assert!(!inflight_clear_invariant_violated(true, false, true));
        // Both → satisfied.
        assert!(!inflight_clear_invariant_violated(true, true, true));
        // No committed delivery → nothing to violate (empty-turn / suppressed cleanup).
        assert!(!inflight_clear_invariant_violated(false, false, false));
    }

    #[test]
    fn turn_identity_string_format() {
        assert_eq!(turn_identity_string(123, 456), "discord:123:456");
        // id-0 (external/injected/TUI-direct) turns keep a stable, JOINable key.
        assert_eq!(turn_identity_string(123, 0), "discord:123:0");
    }

    #[test]
    fn terminal_body_commit_extra_carries_both_owner_signals_split() {
        let extra = terminal_body_commit_extra("discord:1:2", 2, 7, 42, 100, 250, "watcher", true);
        assert_eq!(extra["turn_identity"], "discord:1:2");
        assert_eq!(extra["user_msg_id"], 2);
        assert_eq!(extra["finalizer_turn_id"], 7);
        assert_eq!(extra["status_message_id"], 42);
        assert_eq!(extra["committed_range"], json!([100, 250]));
        // The two owner signals are SEPARATE fields, never collapsed into one.
        assert_eq!(extra["inflight_relay_owner"], "watcher");
        assert!(extra["ledger_relay_owner"].is_null());
        assert_eq!(extra["terminal_delivery_committed"], true);
    }

    #[test]
    fn terminal_ui_transition_extra_fields() {
        let extra = terminal_ui_transition_extra(
            TerminalUiOutcome::GateSuppressed,
            "TimedOut",
            Some(false),
            None,
        );
        assert_eq!(extra["outcome"], "gate_suppressed");
        assert_eq!(extra["gate_outcome"], "TimedOut");
        assert_eq!(extra["pane_quiescent"], false);
        assert!(extra["obligation_id"].is_null());

        let extra2 = terminal_ui_transition_extra(
            TerminalUiOutcome::Committed,
            "ConfirmedIdle",
            Some(true),
            Some(99),
        );
        assert_eq!(extra2["outcome"], "committed");
        assert_eq!(extra2["pane_quiescent"], true);
        assert_eq!(extra2["obligation_id"], 99);
    }

    #[test]
    fn inflight_clear_extra_carries_three_signals_and_flag() {
        let extra = inflight_clear_extra(true, false, false, true);
        assert_eq!(extra["terminal_delivery_committed"], true);
        assert_eq!(extra["terminal_ui_committed"], false);
        assert_eq!(extra["terminal_ui_obligation_persisted"], false);
        assert_eq!(extra["invariant_violated"], true);
    }

    #[test]
    fn finalizer_ledger_owner_extra_fields() {
        let extra = finalizer_ledger_owner_extra("watcher", "Terminal", false);
        assert_eq!(extra["ledger_relay_owner"], "watcher");
        assert_eq!(extra["terminal_event"], "Terminal");
        assert_eq!(extra["clear_inflight"], false);
    }
}
