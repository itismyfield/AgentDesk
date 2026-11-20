//! A hook write may not bring a session row whose hosted execution is retired back to an active
//! status; only a turn start under a nonce the row does not already record reopens it.

/// `WHERE` predicate over the locked hook-update row, reading `$2` (incoming status) and `$17`
/// (turn-start nonce); null-safe, so a row without a hosted record never refuses the write.
macro_rules! retired_revival_fence {
    () => {
        "NOT (lower($2) = 'turn_active'
              AND (hosted_execution->>'state') IS NOT DISTINCT FROM 'retired'
              AND status IS DISTINCT FROM 'turn_active'
              AND ($17::TEXT IS NULL OR $17 IS NOT DISTINCT FROM active_turn_nonce))"
    };
}
pub(super) use retired_revival_fence;
