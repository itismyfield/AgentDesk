//! State for one token's cooperative native stop, separate from raw cancellation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

#[derive(Default)]
pub(crate) struct State {
    inner: Mutex<Inner>,
    notice: AtomicBool,
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
