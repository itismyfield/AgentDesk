//! Durable receipt of a Herdr stop that closed a turn before its input started, with its lock-held
//! admit (`pending`), notice ack (`delivered`) and restart clear check. Unknowns stay verbatim.

use super::*;
use crate::services::discord::formatting::ReplaceLongMessageOutcome;
use crate::services::provider::herdr_before_start::CloseBasis;

const PROTOCOL: u32 = 1;
const KIND: &str = "unsubmitted_before_start_stop";
const PENDING: &str = "pending";
const DELIVERED: &str = "delivered";
const ADMIT_CALLER: &str = "inflight::before_start_stop_admission::admit";
const ACK_CALLER: &str = "inflight::before_start_stop_admission::ack";

/// Raw receipt values survive unsupported fields, types and present-null unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(in crate::services::discord) struct BeforeStartReceipt(serde_json::Value);

pub(in crate::services::discord) fn deserialize_receipt<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<BeforeStartReceipt>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    #[cfg(test)]
    let mut value = value;
    #[cfg(test)]
    {
        let mutant = super::super::super::super::receipt_mutant;
        if mutant("raw-type") && !value.is_object() {
            return Err(serde::de::Error::custom("object required"));
        }
        if mutant("raw-null") && value.is_null() {
            return Ok(None);
        }
        if mutant("raw-shape") {
            if let Some(object) = value.as_object_mut() {
                object.remove("future");
            }
        }
    }
    Ok(Some(BeforeStartReceipt(value)))
}

impl BeforeStartReceipt {
    pub(in crate::services::discord) fn supported(&self) -> bool {
        (super::super::super::super::receipt_mutant("unknown-field")
            || self.0.as_object().is_some_and(|object| object.len() == 6))
            && self.0["protocol"].as_u64() == Some(u64::from(PROTOCOL))
            && self.0["kind"].as_str() == Some(KIND)
            && matches!(self.0["basis"].as_str(), Some("no_attempt" | "untouched"))
            && matches!(self.0["notice"].as_str(), Some(PENDING | DELIVERED))
            && self.0["admitted_generation"].as_u64().is_some()
            && !self.turn_nonce().is_empty()
    }
    pub(in crate::services::discord) fn turn_nonce(&self) -> &str {
        self.0["turn_nonce"].as_str().unwrap_or("")
    }
    pub(in crate::services::discord) fn admitted_generation(&self) -> u64 {
        self.0["admitted_generation"].as_u64().unwrap_or(0)
    }
    #[cfg(unix)]
    pub(in crate::services::discord) fn notice_pending(&self) -> bool {
        self.supported() && self.0["notice"].as_str() == Some(PENDING)
    }
    fn same_admission(&self, other: &Self) -> bool {
        [
            "protocol",
            "kind",
            "turn_nonce",
            "admitted_generation",
            "basis",
        ]
        .iter()
        .all(|key| self.0[*key] == other.0[*key])
    }
}

thread_local! {
    static RECEIPT_WRITE: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) };
}
struct ReceiptWrite(Option<&'static str>);
impl Drop for ReceiptWrite {
    fn drop(&mut self) {
        RECEIPT_WRITE.set(self.0);
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
    let _permission = ReceiptWrite(RECEIPT_WRITE.replace(Some(caller)));
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
    pub(in crate::services::discord::inflight) fn before_start_policy_receipt_clearable(
        &self,
        fresh: &InflightTurnState,
        generation: u64,
    ) -> bool {
        self.herdr_before_start_stop
            .as_ref()
            .is_some_and(|receipt| {
                receipt.supported()
                    && receipt.0["notice"].as_str() == Some(DELIVERED)
                    && receipt.admitted_generation() == generation
                    && generation != 0
                    && fresh.herdr_before_start_stop.as_ref() == Some(receipt)
                    && fresh.turn_nonce.as_deref() == Some(receipt.turn_nonce())
                    && fresh.restart_mode.is_none()
                    && !fresh.rebind_origin
                    && fresh.tui_terminal_kind.is_none()
                    && !fresh.terminal_delivery_committed
                    && fresh.save_generation == self.save_generation
                    && InflightEpisodePin::from_state(self).matches_state(fresh)
            })
    }

    /// General writers carry only the same durable episode's exact receipt.
    pub(in crate::services::discord::inflight) fn carry_before_start_receipt_from(
        &mut self,
        existing: Option<&InflightTurnState>,
    ) -> Result<(), String> {
        if super::super::super::super::receipt_mutant("absent-blocks")
            && self.herdr_before_start_stop.is_none()
        {
            return Err("receipt required".into());
        }
        let durable = existing.and_then(|row| row.herdr_before_start_stop.as_ref());
        if durable.is_some()
            && existing.is_some_and(|row| {
                !InflightEpisodePin::from_state(row)
                    .is_same_episode_as(&InflightEpisodePin::from_state(self))
            })
        {
            return Err("policy receipt belongs to another episode".into());
        }
        let permission = RECEIPT_WRITE.get();
        let outgoing = self.herdr_before_start_stop.as_ref();
        if permission == Some(ADMIT_CALLER)
            && durable.is_none()
            && outgoing.is_some_and(|receipt| receipt.supported())
        {
            return Ok(());
        }
        if permission == Some(ACK_CALLER)
            && durable.zip(outgoing).is_some_and(|(old, new)| {
                old.supported()
                    && new.supported()
                    && old.same_admission(new)
                    && new.0["notice"].as_str() == Some(DELIVERED)
            })
        {
            return Ok(());
        }
        if durable.is_none() && outgoing.is_some() {
            if super::super::super::super::receipt_mutant("mint") {
                return Ok(());
            }
            return Err("policy receipt requires admission authority".into());
        }
        if super::super::super::super::receipt_mutant("raise") {
            return Ok(());
        }
        self.herdr_before_start_stop = durable.cloned();
        Ok(())
    }

    /// Admits a `pending` receipt on the locked fresh row of this birth episode and returns that
    /// row as stored. Any refusal writes nothing and leaves the turn held.
    pub(in crate::services::discord) async fn admit_before_start_receipt(
        &self,
        nonce: &str,
        basis: CloseBasis,
        generation: u64,
    ) -> Option<InflightTurnState> {
        let receipt = BeforeStartReceipt(serde_json::json!({
            "protocol": PROTOCOL, "kind": KIND, "turn_nonce": nonce,
            "admitted_generation": generation,
            "basis": match basis { CloseBasis::NoAttempt => "no_attempt", CloseBasis::Untouched => "untouched" },
            "notice": PENDING,
        }));
        let row = self.clone();
        off_scheduler(move || row.admit_locked(receipt)).await
    }

    fn admit_locked(&self, receipt: BeforeStartReceipt) -> Option<InflightTurnState> {
        if !crate::services::provider::cancel_token_claude_interrupt::herdr_stop_settlement_available()
            || receipt.admitted_generation() == 0
            || receipt.turn_nonce().is_empty()
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
        if fresh.turn_nonce.as_deref() != Some(receipt.turn_nonce())
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
        if fresh.turn_nonce.as_deref() != Some(durable.turn_nonce())
            || fresh.tui_terminal_kind.is_some()
            || fresh.terminal_delivery_committed
        {
            return None;
        }
        if durable.0["notice"].as_str() == Some(DELIVERED) {
            return Some(fresh.clone());
        }
        let mut updated = fresh.clone();
        updated.herdr_before_start_stop.as_mut()?.0["notice"] = DELIVERED.into();
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
            && receipt.0["notice"].as_str() == Some(DELIVERED)
            && fresh.herdr_before_start_stop.as_ref() == Some(receipt)
            && fresh.turn_nonce.as_deref() == Some(receipt.turn_nonce())
            && receipt.admitted_generation() != 0
            && receipt.admitted_generation() < current_generation
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
