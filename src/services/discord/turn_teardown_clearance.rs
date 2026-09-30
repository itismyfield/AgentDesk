//! A provider turn's tmux teardown verdict, judged once before spawn against the
//! sessions row the turn's own writer posted under its exact key.

use sqlx::PgPool;

use crate::services::provider::ProviderKind;
use crate::services::provider_teardown::TeardownClearance;

/// `None` when the turn has no tmux session, so no teardown site runs. Call it after
/// the writer post, inflight save and busy preflight, right before spawn.
pub(super) async fn for_turn(
    pool: Option<&PgPool>,
    provider: &ProviderKind,
    channel_id: u64,
    session_key: Option<&str>,
    tmux_name: Option<&str>,
) -> Option<TeardownClearance> {
    let tmux_name = tmux_name?;
    let Some(session_key) = session_key else {
        tracing::warn!(
            tmux_name,
            channel_id,
            "provider turn has no session key; its tmux teardown stays name-only"
        );
        return Some(TeardownClearance::Unkeyed);
    };
    let cleared = super::inflight::clear_channel_session(
        pool,
        provider,
        channel_id,
        Some(session_key),
        tmux_name,
        "provider_turn_teardown",
    );
    Some(match cleared.await {
        Some(session) => TeardownClearance::Cleared(session),
        None => TeardownClearance::Refused(format!(
            "sessions row {session_key} is not a found legacy row"
        )),
    })
}

#[cfg(test)]
#[path = "turn_teardown_clearance_tests.rs"]
mod tests;
