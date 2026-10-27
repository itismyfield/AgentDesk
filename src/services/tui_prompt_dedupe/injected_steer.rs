//! Nonces AgentDesk pasted into a running Codex turn, recorded before the paste so an observer that
//! meets the input knows it is an injected Discord message and which turn it was aimed at.

use super::*;

/// Unsettled entries per session; a further paste is refused rather than evicting one.
const MAX_PENDING: usize = 8;
/// Settled entries kept per session so a later observer of the same input reuses the result.
const MAX_SETTLED: usize = 32;
/// Every entry, settled or not, is forgotten this long after its paste; later sightings take the
/// ordinary observation path.
pub(super) const INJECTED_STEER_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug)]
pub(super) struct InjectedSteer {
    nonce: String,
    target_turn: String,
    message_id: Option<u64>,
    settled: Option<Settled>,
    retained: bool,
}

/// How the first observation with a native turn settled the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Settled {
    /// It joined a turn that already had an owner, which answers it.
    Joined,
    /// It opened an unowned turn and took the ordinary direct-input path.
    Handed,
}

/// What the observation does with an input the ledger recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Claim {
    /// No native turn yet; nothing is settled until an observer names one.
    Deferred,
    /// Joined an owned turn now; `first` is false for a later sighting of a settled input.
    Joined { first: bool },
    /// Opened an unowned turn now: the ordinary direct-input path runs once.
    Handed,
    /// Already handed: a later sighting is a duplicate.
    HandedBefore,
}

fn marker(nonce: &str) -> String {
    format!(" · {nonce}]")
}

/// Projection of durable retention: frees capacity without inventing a native observation.
#[cfg(test)]
pub(crate) fn retain_injected_steer(provider: &str, session: &str, nonce: &str) {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(ledger) = state
        .injected_steer_by_tmux
        .get_mut(&PromptKey::new(provider, session))
        && let Some(entry) = ledger.iter_mut().find(|entry| entry.value.nonce == nonce)
    {
        entry.value.retained = true;
    }
}

/// Records an injected input before its paste; false when the session already holds the maximum
/// of unsettled inputs or the nonce, so the caller does not paste.
pub(crate) fn register_injected_steer(
    provider: &str,
    tmux_session_name: &str,
    nonce: &str,
    target_turn: &str,
    message_id: Option<u64>,
) -> bool {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let ledger = state
        .injected_steer_by_tmux
        .entry(PromptKey::new(provider, tmux_session_name))
        .or_default();
    let pending = ledger
        .iter()
        .filter(|entry| entry.value.settled.is_none() && !entry.value.retained);
    if nonce.is_empty()
        || pending.count() >= MAX_PENDING
        || ledger.iter().any(|entry| entry.value.nonce == nonce)
    {
        return false;
    }
    ledger.push_back(TimedValue {
        value: InjectedSteer {
            nonce: nonce.to_string(),
            target_turn: target_turn.to_string(),
            message_id,
            settled: None,
            retained: false,
        },
        recorded_at: Instant::now(),
    });
    true
}

/// Forgets an unsettled input whose paste never ran.
pub(crate) fn withdraw_injected_steer(provider: &str, tmux_session_name: &str, nonce: &str) {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    let key = PromptKey::new(provider, tmux_session_name);
    let Some(ledger) = state.injected_steer_by_tmux.get_mut(&key) else {
        return;
    };
    ledger.retain(|entry| entry.value.nonce != nonce || entry.value.settled.is_some());
    if ledger.is_empty() {
        state.injected_steer_by_tmux.remove(&key);
    }
}

/// Settles the recorded input `prompts` carry, by the native turn its record names. The target
/// turn or an already owned turn joins; any other turn is handed to the direct-input path.
pub(super) fn claim_injected_steer(
    provider: &str,
    tmux_session_name: &str,
    prompts: &[String],
    native_turn: Option<&str>,
    entry_id: Option<&str>,
) -> Option<Claim> {
    let mut state = STATE.lock().unwrap_or_else(|error| error.into_inner());
    state.purge_expired();
    let key = PromptKey::new(provider, tmux_session_name);
    let owned = state
        .native_turn_by_tmux
        .get(&key)
        .map(|slot| slot.value.turn.clone());
    use crate::services::tui_input::human_frame::history::{ObservedAttempt, observed_attempt};
    let observed = observed_attempt(provider, tmux_session_name, prompts, entry_id);
    if provider == "claude" && !matches!(&observed, ObservedAttempt::None) {
        return Some(Claim::Joined { first: false });
    }
    let Some(ledger) = state.injected_steer_by_tmux.get_mut(&key) else {
        return (!matches!(observed, ObservedAttempt::None)).then_some(Claim::Deferred);
    };
    let carries = |entry: &TimedValue<InjectedSteer>| {
        let marker = marker(&entry.value.nonce);
        prompts.iter().any(|prompt| prompt.contains(&marker))
    };
    let entry = match observed {
        ObservedAttempt::Known(id) => {
            let Some(entry) = ledger.iter_mut().find(|entry| entry.value.nonce == id) else {
                return Some(Claim::Deferred);
            };
            Some(entry)
        }
        ObservedAttempt::Pending => return Some(Claim::Deferred),
        ObservedAttempt::None => ledger.iter_mut().find(|entry| carries(entry)),
    }?;
    let claim = match (entry.value.settled, native_turn) {
        (Some(Settled::Joined), _) => return Some(Claim::Joined { first: false }),
        (Some(Settled::Handed), _) => return Some(Claim::HandedBefore),
        (None, None) => return Some(Claim::Deferred),
        (None, Some(turn)) if turn == entry.value.target_turn || owned.as_deref() == Some(turn) => {
            entry.value.settled = Some(Settled::Joined);
            Claim::Joined { first: true }
        }
        (None, Some(_)) => {
            entry.value.settled = Some(Settled::Handed);
            Claim::Handed
        }
    };
    tracing::info!(
        tmux_session_name,
        nonce = %entry.value.nonce,
        target_turn = %entry.value.target_turn,
        observed_turn = native_turn.unwrap_or(""),
        message_id = entry.value.message_id.unwrap_or(0),
        claim = ?claim,
        "injected input observed"
    );
    // The oldest settled entries go first once the session keeps too many.
    let settled = ledger.iter().filter(|entry| entry.value.settled.is_some());
    let mut excess = settled.count().saturating_sub(MAX_SETTLED);
    ledger.retain(|entry| {
        let drop = excess > 0 && entry.value.settled.is_some();
        excess -= usize::from(drop);
        !drop
    });
    Some(claim)
}

impl TuiPromptDedupeState {
    pub(super) fn purge_expired_injected_steers(&mut self, now: Instant) {
        self.injected_steer_by_tmux.retain(|_, ledger| {
            ledger.retain(|entry| {
                entry.value.retained || now.duration_since(entry.recorded_at) <= INJECTED_STEER_TTL
            });
            !ledger.is_empty()
        });
    }
}

#[cfg(test)]
mod retained_tests {
    use super::*;
    #[test]
    fn retained_slot_returns_capacity_without_forgetting_or_faking_late_receipt() {
        let session = "v2-retained-late-receipt";
        for n in 0..9 {
            let nonce = format!("retained-{n}");
            assert!(register_injected_steer(
                "codex",
                session,
                &nonce,
                "turn",
                Some(100 + n)
            ));
            retain_injected_steer("codex", session, &nonce);
        }
        {
            let mut state = STATE.lock().unwrap();
            let entry = &mut state
                .injected_steer_by_tmux
                .get_mut(&PromptKey::new("codex", session))
                .unwrap()[0];
            entry.recorded_at = Instant::now() - INJECTED_STEER_TTL - Duration::from_secs(1);
        }
        assert!(register_injected_steer(
            "codex",
            session,
            "tenth",
            "turn",
            Some(110)
        ));
        let prompts = vec![format!("body{}", marker("retained-0"))];
        assert_eq!(
            claim_injected_steer("codex", session, &prompts, None, None),
            Some(Claim::Deferred),
            "retention never invents a settled native observation"
        );
        assert_eq!(
            claim_injected_steer("codex", session, &prompts, Some("turn"), None),
            Some(Claim::Joined { first: true }),
            "late identity survives retention and the ordinary TTL"
        );
    }
}
