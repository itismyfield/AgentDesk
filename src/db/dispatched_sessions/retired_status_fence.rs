//! A status write may not bring a session row whose hosted execution is retired back to an active
//! status; only a hook turn start under a nonce the row does not already record reopens it.

/// The row's hosted execution is retired and the row is not active; null-safe, so a row without
/// a hosted record never matches.
macro_rules! retired_inactive_row {
    () => {
        "((hosted_execution->>'state') IS NOT DISTINCT FROM 'retired'
          AND status IS DISTINCT FROM 'turn_active')"
    };
}
pub(crate) use retired_inactive_row;

/// `WHERE` predicate over the locked hook-update row, reading `$2` (incoming status) and `$17`
/// (turn-start nonce).
macro_rules! retired_revival_fence {
    () => {
        concat!(
            "NOT (lower($2) = 'turn_active' AND ",
            $crate::db::dispatched_session_canonical_identity::retired_status_fence::retired_inactive_row!(),
            " AND ($17::TEXT IS NULL OR $17 IS NOT DISTINCT FROM active_turn_nonce))"
        )
    };
}
pub(super) use retired_revival_fence;
