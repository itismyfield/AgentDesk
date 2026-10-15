//! Turn mode for a Codex channel O took over mid-turn: confirmed only once that turn ended in its
//! rollout and Legacy holds no unfinished turn for the channel, all under the channel's start lock.

use super::super::{TuiDirectPendingStart, pending_synthetic_start_present, root};
use super::{Retirement, read_optional, records, retire_channel};
use crate::services::discord::inflight::{self, InflightTurnState};
use crate::services::discord::tui_direct_pending_start::channel_lock;
use crate::services::provider::ProviderKind;
use crate::services::tui_o::shadow::{MAX_READ_BYTES, SourceBinding};
use crate::services::tui_o::turn_mode::{self, TurnConfig};
use crate::services::tui_o::writer::input_facts::{InputFacts, TurnState};

#[derive(Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum Confirmation {
    Confirmed,
    /// The boot selection leaves the channel on Legacy turns.
    Unselected,
    /// Nothing changed; the caller asks again on its existing schedule.
    Retry(String),
}

/// Confirms a selected Codex channel once the turn in `source` ended and Legacy holds no unfinished
/// turn, then retires the synthetic residue. A retry changes nothing, so it may precede a claim.
pub(in crate::services::discord) fn confirm_codex_channel(
    channel: u64,
    config: Option<&TurnConfig>,
    source: &SourceBinding,
) -> Confirmation {
    if !config.is_some_and(|config| config.selects(channel)) {
        return Confirmation::Unselected;
    }
    let provider = ProviderKind::Codex;
    let lock = channel_lock(provider.as_str(), channel);
    // Start claims hold this lock, so none can add an inflight row between the read and the confirm.
    let Ok(_claims) = lock.try_lock() else {
        return Confirmation::Retry("a start claim holds the channel".into());
    };
    if turn_mode::transcript_turns(channel) {
        return Confirmation::Confirmed;
    }
    let held = ended(source)
        .err()
        .or_else(|| unfinished(&provider, channel));
    if let Some(detail) = held {
        return Confirmation::Retry(detail);
    }
    match retire_channel(&provider, channel) {
        Ok(Retirement {
            removed,
            retry_pending: false,
        }) => {
            turn_mode::confirm(channel);
            tracing::info!(
                channel,
                removed,
                "[tui_o] Codex turn mode confirmed after its takeover turn"
            );
            Confirmation::Confirmed
        }
        outcome => Confirmation::Retry(format!("retirement left {outcome:?}")),
    }
}

/// The whole rollout ends Idle after a turn fact; metadata alone, an open or unknown turn, or a
/// missing file do not end the takeover turn.
fn ended(source: &SourceBinding) -> Result<(), String> {
    let mut facts = InputFacts::open(source.clone())?;
    let mut through = None;
    loop {
        let fact = facts.poll(MAX_READ_BYTES)?;
        if facts.caught_up()? || through == Some(fact.through) {
            return match fact.state {
                TurnState::Idle if facts.saw_turn_evidence() => Ok(()),
                state => Err(format!("the takeover rollout reads {state:?}")),
            };
        }
        through = Some(fact.through);
    }
}

/// A pending start, durable or in memory, or an inflight row before its terminal delivery.
fn unfinished(provider: &ProviderKind, channel: u64) -> Option<String> {
    if pending_synthetic_start_present(provider.as_str(), channel) {
        return Some("a pending start is held in memory".into());
    }
    match records::<TuiDirectPendingStart>(root(), provider, channel) {
        Ok(pending) if pending.is_empty() => {}
        Ok(_) => return Some("a durable pending start remains".into()),
        Err(error) => return Some(error),
    }
    let path = inflight::inflight_runtime_root()
        .map(|root| inflight::inflight_state_path(&root, provider, channel));
    let row = path.as_deref().map(read_optional).transpose();
    match row.map(Option::flatten) {
        Ok(None) => None,
        Ok(Some(bytes)) => match serde_json::from_slice::<InflightTurnState>(&bytes) {
            Ok(row) if row.terminal_delivery_committed => None,
            Ok(_) => Some("an inflight turn has no terminal delivery yet".into()),
            Err(error) => Some(error.to_string()),
        },
        Err(error) => Some(error),
    }
}

#[cfg(test)]
#[path = "codex_tests.rs"]
mod tests;
