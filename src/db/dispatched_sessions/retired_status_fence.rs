//! A hook write may not bring a session row whose hosted execution is retired back to an active
//! status; only a turn start under a nonce the row does not already record reopens it.

/// `WHERE` predicate over the locked `sessions` row of a hook update, as one SQL literal.
/// Binds it reads: `$2` the incoming status, `$17` the incoming turn-start nonce.
macro_rules! retired_revival_fence {
    () => {
        "NOT (lower($2) IN ('turn_active', 'working')
              AND hosted_execution->>'state' = 'retired'
              AND COALESCE(lower(status), '') NOT IN ('turn_active', 'working')
              AND ($17::TEXT IS NULL OR $17 IS NOT DISTINCT FROM active_turn_nonce))"
    };
}
pub(super) use retired_revival_fence;
