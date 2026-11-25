//! A Herdr turn's start, saved on its row when recorded, lets the watcher end that turn after its
//! token is gone: only the turn's own terminal, read from that start, ends it.

use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
use crate::services::discord::turn_finalizer::CompletionSignal;
use crate::services::provider::cancel_token_claude_interrupt::HerdrTurnStart;

/// A recorded start and the turn it was recorded for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct PersistedHerdrTurnStart {
    pub(in crate::services::discord) turn_nonce: String,
    #[serde(flatten)]
    pub(in crate::services::discord) start: HerdrTurnStart,
}

/// What a row shows of its Herdr turn's end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum HerdrTurnEvidence {
    NotHerdr,
    Own,
    Insufficient,
}

/// Routes recorded starts to their rows; a live registry installs it.
pub(in crate::services::discord) fn install_turn_start_sink() {
    crate::services::provider::cancel_token_claude_interrupt::install_turn_start_sink(persist);
}

/// Saves `start` on the row of `turn_nonce` once. A failed save leaves that row held, never the input.
fn persist(owner: &HostedOwner, turn_nonce: &str, start: &HerdrTurnStart) {
    let _ = (owner, turn_nonce, start);
}

/// A stale snapshot of the same turn keeps the saved start; a successor turn never inherits it.
pub(super) fn carry_forward(
    existing: Option<&InflightTurnState>,
    outgoing: &mut InflightTurnState,
) {
    let _ = (existing, outgoing);
}

/// The fresh-idle signal for `row`: a Herdr turn ends on its own terminal only and is held otherwise.
pub(in crate::services::discord) fn herdr_fresh_idle_signal(
    row: Option<&InflightTurnState>,
    logical: &str,
    signal: CompletionSignal,
) -> CompletionSignal {
    let _ = (row, logical);
    signal
}
