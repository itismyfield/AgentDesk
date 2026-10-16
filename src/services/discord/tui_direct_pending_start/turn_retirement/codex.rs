//! Turn mode for a Codex channel O took over mid-turn: confirmed only once that turn ended in its
//! rollout and Legacy holds no unfinished turn for the channel, all under the channel's start lock.

use super::super::{TuiDirectPendingStart, pending_synthetic_start_present, root};
use super::{Retirement, read_optional, records, retire_channel};
use crate::services::discord::inflight::{self, InflightTurnState};
use crate::services::discord::tui_direct_pending_start::channel_lock;
use crate::services::provider::ProviderKind;
use crate::services::tui_o::shadow::{MAX_READ_BYTES, ShadowProvider, SourceBinding};
use crate::services::tui_o::store::OStore;
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

/// Confirms each selected channel of `owned`; a refused one keeps Legacy turns for now.
pub(in crate::services::discord) fn confirm_codex_channels(
    config: Option<&TurnConfig>,
    owned: impl FnOnce() -> Vec<u64>,
) -> Vec<u64> {
    let Some(config) = config.filter(|c| c.all_owned || !c.channels.is_empty()) else {
        return Vec::new();
    };
    let confirmed = |&channel: &u64| match confirm_current(channel, config) {
        Confirmation::Confirmed => true,
        Confirmation::Unselected => false,
        Confirmation::Retry(detail) => {
            tracing::info!(
                channel,
                detail,
                "[tui_o] Codex turn mode waits; Legacy keeps turns"
            );
            false
        }
    };
    owned().into_iter().filter(confirmed).collect()
}

/// A late takeover waits for its turn in the rollout its init committed, before or after a restart;
/// any other committed channel waits only for Legacy's unfinished turn.
pub(in crate::services::discord) fn confirm_current(
    channel: u64,
    config: &TurnConfig,
) -> Confirmation {
    if !config.selects(channel) {
        return Confirmation::Unselected;
    }
    let late = crate::services::tui_o::cutover::boot_ownership()
        .into_iter()
        .any(|(c, _, candidate)| c == channel && candidate.is_some_and(|c| c.late_adopt_origin()));
    // After a restart the candidate is plainly committed; only its init still names a takeover.
    if !late && !crate::services::tui_o::cutover::late_adopt_boot() {
        return confirm_codex_channel(channel, Some(config), None);
    }
    match takeover_source(channel) {
        Ok(Some(source)) => confirm_codex_channel(channel, Some(config), Some(&source)),
        Ok(None) if !late => confirm_codex_channel(channel, Some(config), None),
        Ok(None) => Confirmation::Retry("the takeover init names no single source".into()),
        Err(detail) => Confirmation::Retry(detail),
    }
}

/// The source a takeover's init committed from its first byte, whatever the binding log binds now;
/// None when the init holds anything else.
fn takeover_source(channel: u64) -> Result<Option<SourceBinding>, String> {
    let store = crate::config::runtime_root().and_then(|root| OStore::existing(&root));
    let init = store.ok_or("no O store")?.read_init(channel);
    let sources = init
        .map_err(|error| format!("{error:?}"))?
        .map(|init| init.sources);
    let Some([source]) = sources.as_deref() else {
        return Ok(None);
    };
    Ok((source.delivery_start == 0).then(|| SourceBinding {
        channel_id: channel,
        provider: ShadowProvider::Codex,
        source: source.source_id.clone(),
    }))
}

/// Confirms a selected Codex channel once the turn in `takeover`, when given, ended and Legacy
/// holds no unfinished turn, then retires the synthetic residue. A retry changes nothing.
pub(in crate::services::discord) fn confirm_codex_channel(
    channel: u64,
    config: Option<&TurnConfig>,
    takeover: Option<&SourceBinding>,
) -> Confirmation {
    if !config.is_some_and(|config| config.selects(channel)) {
        return Confirmation::Unselected;
    }
    let provider = ProviderKind::Codex;
    let lock = channel_lock(provider.as_str(), channel);
    // Synthetic start claims take this lock, so none lands between the population read and confirm.
    let Ok(_claims) = lock.try_lock() else {
        return Confirmation::Retry("a start claim holds the channel".into());
    };
    if turn_mode::transcript_turns(channel) {
        return Confirmation::Confirmed;
    }
    let held = takeover
        .and_then(|source| ended(source).err())
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
    let mut read = None;
    loop {
        let fact = facts.poll(MAX_READ_BYTES)?;
        // A record longer than one poll moves only the read position, not `fact.through`.
        let now = facts.observation_progress().0;
        if facts.caught_up()? || read == Some(now) {
            return match fact.state {
                TurnState::Idle if facts.saw_turn_evidence() => Ok(()),
                state => Err(format!("the takeover rollout reads {state:?}")),
            };
        }
        read = Some(now);
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
