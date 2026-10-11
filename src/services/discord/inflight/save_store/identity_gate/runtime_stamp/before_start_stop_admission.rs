//! The durable receipt of a Herdr stop that closed a turn before its input started, and its three
//! lock-held transitions: admit (`pending`), acknowledge the shown notice (`delivered`), and the
//! restart clear check. A value this binary does not know is kept verbatim and never read as known.

use super::*;
use crate::services::discord::formatting::ReplaceLongMessageOutcome;
use crate::services::provider::herdr_before_start::CloseBasis;

const PROTOCOL: u32 = 1;
const KIND: &str = "unsubmitted_before_start_stop";
const PENDING: &str = "pending";
const DELIVERED: &str = "delivered";
const ADMIT_CALLER: &str = "inflight::before_start_stop_admission::admit";
const ACK_CALLER: &str = "inflight::before_start_stop_admission::ack";

/// Missing fields read as unknown values, so a malformed receipt never fails the whole row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(in crate::services::discord) struct BeforeStartReceipt {
    protocol: u32,
    kind: String,
    turn_nonce: String,
    /// The process generation that admitted the close; restart takes only a prior one.
    admitted_generation: u64,
    basis: String,
    notice: String,
}

impl BeforeStartReceipt {
    /// Every field is one this binary reads; generation and row checks are the caller's.
    pub(in crate::services::discord) fn supported(&self) -> bool {
        self.protocol == PROTOCOL
            && self.kind == KIND
            && matches!(self.basis.as_str(), "no_attempt" | "untouched")
            && matches!(self.notice.as_str(), PENDING | DELIVERED)
            && !self.turn_nonce.is_empty()
    }

    #[cfg(unix)]
    pub(in crate::services::discord) fn turn_nonce(&self) -> &str {
        &self.turn_nonce
    }

    #[cfg(unix)]
    pub(in crate::services::discord) fn admitted_generation(&self) -> u64 {
        self.admitted_generation
    }

    #[cfg(unix)]
    pub(in crate::services::discord) fn notice_pending(&self) -> bool {
        self.supported() && self.notice == PENDING
    }

    /// The same admission, whatever its notice says.
    fn same_admission(&self, other: &Self) -> bool {
        self.protocol == other.protocol
            && self.kind == other.kind
            && self.turn_nonce == other.turn_nonce
            && self.admitted_generation == other.admitted_generation
            && self.basis == other.basis
    }

    /// Unknown values rank above every known one so no write lowers them.
    fn rank(&self) -> u8 {
        match (self.supported(), self.notice.as_str()) {
            (true, PENDING) => 1,
            (true, DELIVERED) => 2,
            _ => u8::MAX,
        }
    }
}

/// Only a real send counts; a partial continuation failure is not a shown notice.
fn notice_delivered(sent: &Result<ReplaceLongMessageOutcome, String>) -> bool {
    matches!(
        sent,
        Ok(ReplaceLongMessageOutcome::EditedOriginal
            | ReplaceLongMessageOutcome::SentFallbackAfterEditFailure { .. })
    )
}

/// Runs a lock-held transition on the blocking pool, so no inflight lock spans an await. Tests
/// carry this thread's settlement switch over, since it is thread-local.
async fn off_scheduler<T: Send + 'static>(
    transition: impl FnOnce() -> Option<T> + Send + 'static,
) -> Option<T> {
    #[cfg(all(test, unix))]
    let settlement =
        crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE
            .with(std::cell::Cell::get);
    tokio::task::spawn_blocking(move || {
        #[cfg(all(test, unix))]
        crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE
            .set(settlement);
        transition()
    })
    .await
    .ok()
    .flatten()
}

/// `Some` only when the durable row now holds exactly `updated`'s receipt.
fn commit(
    guard: &mut LockedInflightEpisode,
    updated: &InflightTurnState,
    caller: &'static str,
) -> Option<InflightTurnState> {
    #[cfg(all(test, unix))]
    fault::check(guard.state(), caller).ok()?;
    match guard.persist_verified_under_guard(updated, caller) {
        GuardedSaveOutcome::Saved
            if guard.state().herdr_before_start_stop == updated.herdr_before_start_stop =>
        {
            Some(guard.state().clone())
        }
        _ => None,
    }
}

impl InflightTurnState {
    /// Every row write keeps the durable receipt of the same birth episode: a stale snapshot can
    /// neither drop nor lower it, and only its own acknowledgement raises it.
    pub(in crate::services::discord::inflight) fn carry_before_start_receipt_from(
        &mut self,
        existing: Option<&InflightTurnState>,
    ) {
        let Some(durable) = existing
            .filter(|existing| {
                InflightEpisodePin::from_state(existing)
                    .is_same_episode_as(&InflightEpisodePin::from_state(self))
            })
            .and_then(|existing| existing.herdr_before_start_stop.as_ref())
        else {
            return;
        };
        self.herdr_before_start_stop = match self.herdr_before_start_stop.take() {
            Some(next) if next.same_admission(durable) && next.rank() > durable.rank() => {
                Some(next)
            }
            _ => Some(durable.clone()),
        };
    }

    /// Admits a `pending` receipt on the locked fresh row of this birth episode and returns that
    /// row as stored. Any refusal writes nothing and leaves the turn held.
    pub(in crate::services::discord) async fn admit_before_start_receipt(
        &self,
        nonce: &str,
        basis: CloseBasis,
        generation: u64,
    ) -> Option<InflightTurnState> {
        let receipt = BeforeStartReceipt {
            protocol: PROTOCOL,
            kind: KIND.into(),
            turn_nonce: nonce.into(),
            admitted_generation: generation,
            basis: match basis {
                CloseBasis::NoAttempt => "no_attempt",
                CloseBasis::Untouched => "untouched",
            }
            .into(),
            notice: PENDING.into(),
        };
        let row = self.clone();
        off_scheduler(move || row.admit_locked(receipt)).await
    }

    fn admit_locked(&self, receipt: BeforeStartReceipt) -> Option<InflightTurnState> {
        if !crate::services::provider::cancel_token_claude_interrupt::herdr_stop_settlement_available()
            || receipt.admitted_generation == 0
            || receipt.turn_nonce.is_empty()
        {
            return None;
        }
        let provider = self.provider_kind()?;
        let pin = InflightEpisodePin::from_state(self);
        let mut guard =
            crate::services::discord::inflight::episode_guard::lock_inflight_birth_episode(
                &provider,
                self.channel_id,
                &pin,
            )
            .ok()?;
        let fresh = guard.state();
        if fresh.turn_nonce.as_deref() != Some(receipt.turn_nonce.as_str())
            || !InflightTurnIdentity::from_state(self).matches_state(fresh)
            || fresh.effective_finalizer_turn_id() != self.effective_finalizer_turn_id()
            || fresh.rebind_origin
            || fresh.restart_mode.is_some()
            || fresh.tui_terminal_kind.is_some()
            || fresh.terminal_delivery_committed
            || !StreamRelayAuthority::from_state(fresh).bridge_owns_relay()
        {
            return None;
        }
        match &fresh.herdr_before_start_stop {
            Some(durable) if durable.same_admission(&receipt) => return Some(fresh.clone()),
            Some(_) => return None,
            None => {}
        }
        let mut updated = fresh.clone();
        updated.herdr_before_start_stop = Some(receipt);
        commit(&mut guard, &updated, ADMIT_CALLER)
    }

    /// Marks this row's receipt `delivered` only for a notice that was really shown, on the locked
    /// fresh row that still holds the same admission; returns that row as stored.
    pub(in crate::services::discord) async fn ack_before_start_notice(
        &self,
        sent: &Result<ReplaceLongMessageOutcome, String>,
    ) -> Option<InflightTurnState> {
        if !notice_delivered(sent) {
            return None;
        }
        let row = self.clone();
        off_scheduler(move || row.ack_locked()).await
    }

    fn ack_locked(&self) -> Option<InflightTurnState> {
        let expected = self.herdr_before_start_stop.as_ref()?;
        let provider = self.provider_kind()?;
        let pin = InflightEpisodePin::from_state(self);
        let mut guard =
            crate::services::discord::inflight::episode_guard::lock_inflight_birth_episode(
                &provider,
                self.channel_id,
                &pin,
            )
            .ok()?;
        let fresh = guard.state();
        let durable = fresh
            .herdr_before_start_stop
            .as_ref()
            .filter(|durable| durable.supported() && durable.same_admission(expected))?;
        if fresh.turn_nonce.as_deref() != Some(durable.turn_nonce.as_str())
            || fresh.tui_terminal_kind.is_some()
            || fresh.terminal_delivery_committed
        {
            return None;
        }
        if durable.notice == DELIVERED {
            return Some(fresh.clone());
        }
        let mut updated = fresh.clone();
        updated.herdr_before_start_stop.as_mut()?.notice = DELIVERED.into();
        commit(&mut guard, &updated, ACK_CALLER)
    }

    /// Restart may clear `fresh` only while it is still exactly this snapshot's delivered receipt
    /// row, admitted and born before `current_generation`.
    #[cfg(unix)]
    pub(in crate::services::discord::inflight) fn before_start_receipt_clearable(
        &self,
        fresh: &InflightTurnState,
        current_generation: u64,
    ) -> bool {
        let Some(receipt) = self.herdr_before_start_stop.as_ref() else {
            return false;
        };
        receipt.supported()
            && receipt.notice == DELIVERED
            && fresh.herdr_before_start_stop.as_ref() == Some(receipt)
            && fresh.turn_nonce.as_deref() == Some(receipt.turn_nonce.as_str())
            && receipt.admitted_generation != 0
            && receipt.admitted_generation < current_generation
            && !crate::services::discord::inflight::row_is_current_generation(
                fresh,
                current_generation,
            )
            && fresh.restart_mode.is_none()
            && !fresh.rebind_origin
            && fresh.tui_terminal_kind.is_none()
            && !fresh.terminal_delivery_committed
            && fresh.save_generation == self.save_generation
            && InflightEpisodePin::from_state(self).matches_state(fresh)
    }
}

#[cfg(all(test, unix))]
#[path = "before_start_stop_admission_tests.rs"]
mod tests;

/// Fault injection for receipt writes, keyed by channel so parallel tests stay apart.
#[cfg(all(test, unix))]
mod fault {
    use crate::services::discord::inflight::InflightTurnState;
    use std::sync::Mutex;

    /// Armed channels; `true` writes the temp file and fails its rename, `false` fails the write.
    static FAULTS: Mutex<Vec<(u64, bool)>> = Mutex::new(Vec::new());

    impl InflightTurnState {
        pub(in crate::services::discord) fn arm_before_start_receipt_fault(
            channel_id: u64,
            rename: bool,
        ) {
            FAULTS.lock().unwrap().push((channel_id, rename));
        }

        /// Whether every armed fault of `channel_id` was consumed by a receipt write.
        pub(in crate::services::discord) fn before_start_receipt_faults_consumed(
            channel_id: u64,
        ) -> bool {
            !FAULTS
                .lock()
                .unwrap()
                .iter()
                .any(|(channel, _)| *channel == channel_id)
        }
    }

    /// Fails an armed channel's receipt write before the row is published.
    pub(super) fn check(row: &InflightTurnState, caller: &str) -> Result<(), String> {
        let rename = {
            let mut faults = FAULTS.lock().unwrap();
            let index = faults
                .iter()
                .position(|(channel, _)| *channel == row.channel_id);
            index.map(|index| faults.remove(index).1)
        };
        match rename {
            None => Ok(()),
            Some(false) => Err(format!("{caller}: injected write fault")),
            Some(true) => {
                let root = crate::services::discord::inflight::inflight_runtime_root()
                    .ok_or("no runtime root")?;
                let provider = row.provider_kind().ok_or("unknown provider")?;
                let path = crate::services::discord::inflight::inflight_state_path(
                    &root,
                    &provider,
                    row.channel_id,
                );
                std::fs::write(path.with_extension("json.receipt.tmp"), b"{}")
                    .map_err(|error| error.to_string())?;
                Err(format!("{caller}: injected rename fault"))
            }
        }
    }
}
