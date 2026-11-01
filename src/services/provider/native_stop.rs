//! State for one token's cooperative native stop, separate from raw cancellation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

#[derive(Default)]
pub(crate) struct State {
    inner: Mutex<Inner>,
    notice: AtomicBool,
    bridges: Mutex<Vec<Weak<()>>>,
    input: Arc<ReceiptSink>,
}

/// Native UUID, open file identity, complete byte range and exact row hash.
pub(crate) type NativeUserPin = (String, (u64, u64), (u64, u64), u64);

/// The writer's receipt is observational; it grants no cancellation authority.
#[derive(Default)]
pub(crate) struct ReceiptSink {
    inner: Mutex<WriterReceipt>,
}
#[derive(Default)]
struct WriterReceipt {
    receipt: Option<crate::services::claude_tui::input::UserReceipt>,
    applied: bool,
    observed: Option<Result<NativeUserPin, ()>>,
}

impl ReceiptSink {
    pub(crate) fn receipt(&self, receipt: crate::services::claude_tui::input::UserReceipt) {
        let mut input = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !input.applied && input.observed.is_none() {
            input.receipt = Some(receipt);
        }
    }
    pub(crate) fn finished(&self, run: &crate::services::claude_tui::host_input::InputRun) {
        let mut input = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if input.applied || input.observed.is_some() {
            return;
        }
        if *run == crate::services::claude_tui::host_input::InputRun::Applied {
            input.applied = true;
            if let Some(receipt) = &mut input.receipt {
                receipt.seal();
            }
        } else {
            input.observed = Some(Err(()));
        }
    }
    pub(crate) fn observe(&self) -> Option<Result<NativeUserPin, ()>> {
        let mut input = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if input.applied && input.observed.is_none() {
            input.observed = match input.receipt.as_mut() {
                Some(receipt) => receipt.try_pin_native_user().transpose(),
                None => Some(Err(())),
            };
        }
        input.observed.clone()
    }

    #[cfg(test)]
    pub(crate) fn observed(&self) -> Option<Result<NativeUserPin, ()>> {
        self.inner.lock().unwrap().observed.clone()
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
