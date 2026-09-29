//! Gateway ownership as O admission sees it. Every lease transition and every O admission take
//! the same lock, so once a change lands no new POST is admitted under the old state.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};

use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GatewayOwnership {
    /// This process holds the gateway lease; `epoch` grows with every acquisition.
    Owned { epoch: u64 },
    /// The lease connection failed and re-acquisition has not settled.
    Unknown,
    /// Never held, handed off, or released.
    Lost,
}

struct GateState {
    ownership: GatewayOwnership,
    last_epoch: u64,
}

pub struct OwnershipGate {
    state: Mutex<GateState>,
    tx: watch::Sender<GatewayOwnership>,
}

impl Default for OwnershipGate {
    fn default() -> Self {
        let ownership = GatewayOwnership::Lost;
        let state = Mutex::new(GateState {
            ownership,
            last_epoch: 0,
        });
        Self {
            state,
            tx: watch::Sender::new(ownership),
        }
    }
}

impl OwnershipGate {
    fn locked(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn set(&self, state: &mut GateState, ownership: GatewayOwnership) {
        state.ownership = ownership;
        self.tx.send_replace(ownership);
    }

    pub fn current(&self) -> GatewayOwnership {
        self.locked().ownership
    }

    pub fn subscribe(&self) -> watch::Receiver<GatewayOwnership> {
        self.tx.subscribe()
    }

    /// The lease was (re)acquired; returns the new epoch.
    pub fn acquired(&self) -> u64 {
        let mut state = self.locked();
        state.last_epoch += 1;
        let epoch = state.last_epoch;
        self.set(&mut state, GatewayOwnership::Owned { epoch });
        epoch
    }

    /// The lease connection failed; admission closes before any re-acquisition attempt.
    pub fn uncertain(&self) {
        let mut state = self.locked();
        if state.ownership != GatewayOwnership::Lost {
            self.set(&mut state, GatewayOwnership::Unknown);
        }
    }

    pub fn lost(&self) {
        let mut state = self.locked();
        self.set(&mut state, GatewayOwnership::Lost);
    }

    /// Runs `hand_off` under the gate lock only while `Owned`; it must hand the request to the
    /// HTTP client without awaiting, so a transition never waits on a response.
    pub fn admit<T>(&self, hand_off: impl FnOnce(u64) -> T) -> Option<T> {
        let state = self.locked();
        match state.ownership {
            GatewayOwnership::Owned { epoch } => Some(hand_off(epoch)),
            GatewayOwnership::Unknown | GatewayOwnership::Lost => None,
        }
    }
}

static GATES: LazyLock<Mutex<HashMap<String, Arc<OwnershipGate>>>> = LazyLock::new(Mutex::default);

/// The process-wide gate of one provider's gateway lease.
pub fn gate(provider: &str) -> Arc<OwnershipGate> {
    let mut gates = GATES.lock().unwrap_or_else(PoisonError::into_inner);
    Arc::clone(gates.entry(provider.to_string()).or_default())
}

/// Every normal lease release goes through here: `Lost` lands before the unlock or drop runs.
pub async fn release_gateway<F: Future>(gate: &OwnershipGate, release: F) -> F::Output {
    gate.lost();
    release.await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_close_admission_and_epochs_grow_per_acquisition() {
        let gate = OwnershipGate::default();
        assert_eq!(gate.admit(|epoch| epoch), None);
        gate.uncertain();
        assert_eq!(
            gate.current(),
            GatewayOwnership::Lost,
            "Unknown needs a lease first"
        );
        assert_eq!(gate.acquired(), 1);
        assert_eq!(gate.admit(|epoch| epoch), Some(1));
        gate.uncertain();
        assert_eq!(gate.admit(|epoch| epoch), None);
        assert_eq!(gate.acquired(), 2);
        let mut watcher = gate.subscribe();
        gate.lost();
        assert!(watcher.has_changed().unwrap());
        assert_eq!(*watcher.borrow_and_update(), GatewayOwnership::Lost);
        assert_eq!(gate.admit(|epoch| epoch), None);
    }

    #[test]
    fn a_transition_waits_for_an_admission_in_progress_and_admits_nothing_after() {
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel::<()>();
        let admitting = Arc::clone(&gate);
        let admission = std::thread::spawn(move || {
            admitting.admit(|epoch| {
                entered_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
                epoch
            })
        });
        entered_rx.recv().unwrap();
        let changing = Arc::clone(&gate);
        let change = std::thread::spawn(move || changing.uncertain());
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !change.is_finished(),
            "the transition overtook an admission"
        );
        resume_tx.send(()).unwrap();
        assert_eq!(admission.join().unwrap(), Some(1));
        change.join().unwrap();
        assert_eq!(gate.admit(|epoch| epoch), None);
    }

    #[tokio::test]
    async fn release_records_lost_before_the_release_runs() {
        let gate = gate("ownership-release-test");
        gate.acquired();
        let seen = release_gateway(&gate, async { gate.current() }).await;
        assert_eq!(seen, GatewayOwnership::Lost);
    }
}
