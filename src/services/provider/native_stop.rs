//! State for one token's cooperative native stop, separate from raw cancellation.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

#[cfg(test)]
#[path = "native_stop/persistence_tests.rs"]
mod persistence_tests;

#[derive(Default)]
pub(crate) struct State {
    inner: Mutex<Inner>,
    notice: AtomicBool,
    bridges: Mutex<Vec<Weak<()>>>,
    input: Arc<ReceiptSink>,
    /// Busy attempts handed into this token's running turn, keyed by their frame ledger id.
    busy: Mutex<Vec<String>>,
}

/// Stable evidence of one native input; no prompt text is persisted here.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct NativeUserPin {
    pub(crate) uuid: String,
    pub(crate) source_file: (u64, u64),
    pub(crate) range: (u64, u64),
    pub(crate) receipt_start: u64,
    pub(crate) row_digest: String,
    pub(crate) frame_digest: String,
    pub(crate) session_id: String,
    pub(crate) output_path: String,
    pub(crate) tmux_session: String,
    pub(crate) spawn_nonce: Option<String>,
    pub(crate) direct: bool,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PinSaveOutcome {
    Saved,
    #[default]
    Retry,
    Never,
}

type PinObserver = Arc<dyn Fn(&NativeUserPin) -> PinSaveOutcome + Send + Sync>;

#[derive(Default)]
struct PinPersistence {
    outcome: PinSaveOutcome,
    retry_after: Option<Instant>,
}

/// The writer's receipt is observational; it grants no cancellation authority.
#[derive(Default)]
pub(crate) struct ReceiptSink {
    inner: Mutex<WriterReceipt>,
    accepted: OnceLock<NativeUserPin>,
    rejected: AtomicBool,
    observer: OnceLock<PinObserver>,
    persistence: Mutex<PinPersistence>,
    /// A durable pin restored unchecked; only a stop accepts it after rechecking the live pane.
    restored: Mutex<Option<NativeUserPin>>,
    /// The fresh writer's first-key gate: ungated, no key yet, stopped before a key, or keyed.
    first_key: AtomicU8,
}

const UNGATED: u8 = 0;
const NO_KEY_YET: u8 = 1;
const STOPPED_BEFORE_KEY: u8 = 2;
const MAY_HAVE_WRITTEN: u8 = 3;
#[derive(Default)]
struct WriterReceipt {
    receipt: Option<crate::services::claude_tui::input::UserReceipt>,
    pane: Option<Arc<crate::services::claude_tui::input::PaneIdentity>>,
    applied: bool,
}

impl ReceiptSink {
    pub(crate) fn receipt(&self, receipt: crate::services::claude_tui::input::UserReceipt) {
        let mut input = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if self.accepted.get().is_some() {
            return;
        }
        let pane = receipt.pane_identity();
        if input.pane.as_ref().is_some_and(|old| **old != *pane) {
            input.applied = false;
            self.rejected.store(false, Ordering::Release);
        }
        if !input.applied && !self.rejected.load(Ordering::Acquire) {
            input.pane = Some(pane);
            input.receipt = Some(receipt);
        }
    }
    pub(crate) fn finished(&self, run: &crate::services::claude_tui::host_input::InputRun) {
        let mut input = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if input.applied || self.cached().is_some() {
            return;
        }
        if *run == crate::services::claude_tui::host_input::InputRun::Applied {
            input.applied = true;
            if let Some(receipt) = &mut input.receipt {
                receipt.seal();
            }
        } else {
            input.receipt = None;
            self.rejected.store(true, Ordering::Release);
        }
    }
    fn cached(&self) -> Option<Result<NativeUserPin, ()>> {
        self.accepted
            .get()
            .cloned()
            .map(Ok)
            .or_else(|| self.rejected.load(Ordering::Acquire).then_some(Err(())))
    }
    pub(crate) fn observe(&self) -> Option<Result<NativeUserPin, ()>> {
        if let Some(result) = self.cached() {
            self.notify(false);
            return Some(result);
        }
        {
            let mut input = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if input.applied && self.cached().is_none() {
                let result = input
                    .receipt
                    .as_mut()
                    .map_or(Err(()), |r| r.try_pin_native_user());
                match result {
                    Ok(Some(pin)) => {
                        let _ = self.accepted.set(pin);
                    }
                    Err(()) => self.rejected.store(true, Ordering::Release),
                    Ok(None) => {}
                }
                if self.cached().is_some() {
                    input.receipt = None;
                }
            }
        }
        // Source and receipt locks are released before a callback takes the row lock.
        self.notify(false);
        self.cached()
    }
    pub(crate) fn adopt(&self, pin: NativeUserPin) {
        {
            let mut input = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if self.cached().is_none() {
                let _ = self.accepted.set(pin);
                input.receipt = None;
            }
        }
        self.notify(false);
    }
    pub(crate) fn restore_candidate(&self, pin: NativeUserPin) {
        if self.cached().is_none() {
            *self.restored.lock().unwrap_or_else(|e| e.into_inner()) = Some(pin);
        }
    }
    /// The pin a stop may target, rechecked against the live pane now; a failed check targets none.
    pub(crate) fn stop_pin(&self) -> Option<NativeUserPin> {
        let live = crate::services::claude_tui::input::revalidate_native_pin;
        match self.observe() {
            Some(Ok(pin)) => return live(&pin).then_some(pin),
            Some(Err(())) => return None,
            None => {}
        }
        let candidate = self
            .restored
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()?;
        if !live(&candidate) {
            return None;
        }
        self.adopt(candidate.clone());
        self.accepted
            .get()
            .filter(|pin| **pin == candidate)
            .cloned()
    }
    /// A fresh writer's token, before it is published: no payload key has gone yet.
    pub(crate) fn arm_first_key(&self) {
        let _ = self.gate(UNGATED, NO_KEY_YET);
    }
    /// The writer's first payload key; refused once a stop took the gate before it.
    pub(crate) fn writer_first_key(&self) -> bool {
        self.gate(NO_KEY_YET, MAY_HAVE_WRITTEN)
            .is_none_or(|current| current != STOPPED_BEFORE_KEY)
    }
    /// A stop before the writer's first key takes the gate so the writer sends none; `None` for
    /// a token no fresh writer armed, `Some(false)` once the writer may have written.
    pub(crate) fn stop_before_key(&self) -> Option<bool> {
        match self.gate(NO_KEY_YET, STOPPED_BEFORE_KEY) {
            None | Some(STOPPED_BEFORE_KEY) => Some(true),
            Some(MAY_HAVE_WRITTEN) => Some(false),
            Some(_) => None,
        }
    }
    /// Moves the gate from `from` to `to`; `Some(current)` when it held another state.
    fn gate(&self, from: u8, to: u8) -> Option<u8> {
        (self.first_key)
            .compare_exchange(from, to, Ordering::AcqRel, Ordering::Acquire)
            .err()
    }
    pub(crate) fn stopped_before_key(&self) -> bool {
        self.first_key.load(Ordering::Acquire) == STOPPED_BEFORE_KEY
    }
    /// The writer's first key went or its input applied: the pane may hold this token's input.
    pub(crate) fn may_have_written(&self) -> bool {
        self.first_key.load(Ordering::Acquire) == MAY_HAVE_WRITTEN
            || self.inner.lock().unwrap_or_else(|e| e.into_inner()).applied
    }
    /// A pin may still come: the writer may have written and no pin was taken or refused.
    pub(crate) fn pin_pending(&self) -> bool {
        self.may_have_written() && self.cached().is_none()
    }
    pub(crate) fn on_pin(&self, observer: PinObserver) {
        let _ = self.observer.set(observer);
        self.notify(false);
    }
    /// The direct reader has no token; its first bridge frame supplies one immediate retry.
    #[cfg(any(unix, test))]
    pub(crate) fn retry_persistence(&self) {
        self.notify(true);
    }
    fn notify(&self, retry_now: bool) {
        let mut state = self.persistence.lock().unwrap_or_else(|e| e.into_inner());
        if state.outcome != PinSaveOutcome::Retry
            || (!retry_now
                && state
                    .retry_after
                    .is_some_and(|after| Instant::now() < after))
        {
            return;
        }
        if let (Some(pin), Some(observer)) = (self.accepted.get(), self.observer.get()) {
            state.outcome = observer(pin);
            state.retry_after = (state.outcome == PinSaveOutcome::Retry)
                .then(|| Instant::now() + Duration::from_secs(1));
        }
    }
    #[cfg(all(test, unix))]
    pub(crate) fn retains_receipt(&self) -> bool {
        self.inner.lock().unwrap().receipt.is_some()
    }
    #[cfg(test)]
    pub(crate) fn observed(&self) -> Option<Result<NativeUserPin, ()>> {
        self.cached()
    }
}

#[derive(Default)]
pub(crate) struct Inner {
    attempt: Option<Arc<Attempt>>,
    completed: bool,
    orphan: bool,
    idle: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

pub(crate) struct Attempt {
    key: String,
    pub(crate) delivered: OnceLock<bool>,
    started: AtomicBool,
    changed: tokio::sync::Notify,
    terminal: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl State {
    pub(crate) fn receipt_sink(&self) -> Arc<ReceiptSink> {
        self.input.clone()
    }

    pub(crate) fn observe_native_input(&self) -> Option<Result<NativeUserPin, ()>> {
        self.input.observe()
    }

    /// Every bridge holds its own lifetime guard, including overlapping recovered bridges.
    pub(crate) fn register_busy_child(&self, attempt: &str) {
        const KEPT: usize = 64;
        let mut busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        if !busy.iter().any(|kept| kept == attempt) {
            if busy.len() >= KEPT {
                busy.remove(0);
            }
            busy.push(attempt.to_string());
        }
    }

    pub(crate) fn is_busy_child(&self, attempt: &str) -> bool {
        let busy = self.busy.lock().unwrap_or_else(|e| e.into_inner());
        busy.iter().any(|kept| kept == attempt)
    }

    pub(crate) fn register_bridge(&self) -> Arc<()> {
        let guard = Arc::new(());
        let mut bridges = self
            .bridges
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        bridges.retain(|bridge| bridge.strong_count() > 0);
        bridges.push(Arc::downgrade(&guard));
        guard
    }

    pub(crate) fn has_live_bridge(&self) -> bool {
        let mut bridges = self
            .bridges
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        bridges.retain(|bridge| bridge.upgrade().is_some());
        !bridges.is_empty()
    }

    pub(crate) fn attempt(
        &self,
        key: String,
        terminal: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Option<Arc<Attempt>> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.completed {
            return None;
        }
        inner.idle = None;
        inner.orphan = false;
        if let Some(attempt) = &inner.attempt {
            if attempt.key != key {
                return None;
            }
            if attempt.delivered.get() != Some(&false) {
                return Some(attempt.clone());
            }
        }
        let attempt = Arc::new(Attempt {
            key,
            terminal,
            delivered: OnceLock::new(),
            started: AtomicBool::new(false),
            changed: tokio::sync::Notify::new(),
        });
        inner.attempt = Some(attempt.clone());
        Some(attempt)
    }

    /// Main's idle recovery keeps a terminal witness through the actor's publication.
    pub(crate) fn handoff_idle(&self, terminal: Arc<dyn Fn() -> bool + Send + Sync>) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.completed {
            return false;
        }
        inner.orphan = false;
        inner.idle = Some(terminal);
        inner.attempt = None;
        true
    }

    /// Recovery rechecks the absence of this token's native owner at actor publication.
    pub(crate) fn handoff_recovery(&self, witness: Arc<dyn Fn() -> bool + Send + Sync>) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !witness() {
            return false;
        }
        inner.idle = Some(witness);
        inner.orphan = true;
        inner.attempt = None;
        true
    }

    pub(crate) fn is_completed(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .completed
    }

    pub(crate) fn completed(&self) {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .completed = true;
    }

    pub(crate) fn admit(&self) -> Result<MutexGuard<'_, Inner>, ()> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner
            .idle
            .as_ref()
            .is_some_and(|terminal| (inner.completed && !inner.orphan) || !terminal())
        {
            return Err(());
        }
        if let Some(attempt) = &inner.attempt
            && (inner.completed
                || (!mutant("unconfirmed_cancel")
                    && (attempt.delivered.get() != Some(&true)
                        || (!mutant("proof_is_terminal") && !(attempt.terminal)()))))
        {
            return Err(());
        }
        // A cancel admitted before a fresh writer's first key leaves that writer no key to send.
        self.input.stop_before_key();
        Ok(inner)
    }

    pub(crate) fn claim_notice(&self) -> bool {
        !self.notice.swap(true, Ordering::AcqRel)
    }
}

impl Attempt {
    pub(crate) fn start(&self) -> bool {
        !self.started.swap(true, Ordering::AcqRel)
    }

    pub(crate) fn complete(&self, delivered: bool) {
        let _ = self.delivered.set(delivered);
        self.changed.notify_waiters();
    }

    pub(crate) async fn wait(&self) -> bool {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(result) = self.delivered.get() {
                return *result;
            }
            changed.await;
        }
    }
}

pub(crate) fn mutant(name: &str) -> bool {
    #[cfg(test)]
    {
        std::env::var("AGENTDESK_I7193_MUTANT").ok().as_deref() == Some(name)
    }
    #[cfg(not(test))]
    {
        let _ = name;
        false
    }
}
