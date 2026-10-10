//! Dormant, process-local registration fences; no turn facts or restart authority live here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use tokio::task::JoinHandle;

use super::activity::Activity;
use super::admission::Incarnation;

pub(crate) struct Runtime(Mutex<State>);

struct State {
    open: bool,
    owner_present: bool,
    owner_token: Arc<()>,
    channels: HashMap<u64, Registration>,
}

impl State {
    fn fresh(&mut self) {
        for registration in self.channels.values_mut().filter(|r| !r.retired) {
            registration.reset();
        }
    }

    fn suspend(&mut self) {
        self.open = false;
        self.fresh();
    }
}

/// Move this guard into the owning task before its first poll; other runtime references may live.
pub(crate) struct Lifetime {
    runtime: Arc<Runtime>,
    token: Arc<()>,
}

impl Drop for Lifetime {
    fn drop(&mut self) {
        let mut state = self.runtime.0.lock().unwrap_or_else(|e| e.into_inner());
        if Arc::ptr_eq(&state.owner_token, &self.token) {
            state.owner_present = false;
            state.suspend();
        }
    }
}

pub(super) struct Registration {
    token: Arc<()>,
    pub(super) incarnation: Arc<Incarnation>,
    pub(super) activity: Activity,
    pub(super) deadline: Option<Instant>,
    children: Vec<JoinHandle<()>>,
    retired: bool,
}

impl Default for Registration {
    fn default() -> Self {
        Self {
            token: Arc::new(()),
            incarnation: Arc::default(),
            activity: Activity::Unknown,
            deadline: None,
            children: Vec::new(),
            retired: false,
        }
    }
}

impl Registration {
    pub(super) fn advance_ticket(&mut self) {
        self.token = Arc::new(());
        self.activity = Activity::Unknown;
        self.deadline = None;
        for child in self.children.drain(..) {
            child.abort();
        }
    }

    fn reset(&mut self) {
        self.incarnation.invalidate();
        *self = Self::default();
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        for child in &self.children {
            child.abort();
        }
        self.incarnation.invalidate();
    }
}

/// Weak runtime ownership keeps a child from extending its supervisor's lifetime.
#[derive(Clone)]
pub(crate) struct Ticket {
    runtime: Weak<Runtime>,
    channel: u64,
    token: Arc<()>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self(Mutex::new(State {
            open: true,
            owner_present: false,
            owner_token: Arc::new(()),
            channels: HashMap::new(),
        }))
    }
}

impl Runtime {
    pub(crate) fn own(self: &Arc<Self>) -> Lifetime {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.owner_token = Arc::new(());
        state.owner_present = true;
        state.fresh();
        Lifetime {
            runtime: self.clone(),
            token: state.owner_token.clone(),
        }
    }

    pub(crate) fn register(self: &Arc<Self>, channel: u64) -> Option<Ticket> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if !state.open || !state.owner_present || channel == 0 {
            return None;
        }
        let registration = state.channels.entry(channel).or_default();
        if registration.retired {
            *registration = Registration::default();
        }
        Some(Ticket {
            runtime: Arc::downgrade(self),
            channel,
            token: registration.token.clone(),
        })
    }

    pub(crate) fn invalidate_if_current(&self, ticket: &Ticket, cause: &'static str) -> bool {
        ticket.transition(self, |registration| {
            registration.reset();
            tracing::debug!(
                channel = ticket.channel,
                cause,
                "presence approval invalidated"
            );
        })
    }

    pub(crate) fn retire_if_current(&self, ticket: &Ticket) -> bool {
        ticket.transition(self, |registration| {
            registration.reset();
            registration.retired = true;
        })
    }

    pub(crate) fn prune(&self) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .channels
            .retain(|_, registration| !registration.retired);
    }

    pub(crate) fn suspend_runtime(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).suspend();
    }

    /// Only the existing restart decision may call this; no prior authority or timer is restored.
    pub(crate) fn resume_fresh(&self) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.fresh();
        state.open = true;
    }
}

impl Ticket {
    pub(super) fn with_current<R>(
        &self,
        hand_off: impl FnOnce(&mut Registration) -> R,
    ) -> Option<R> {
        let runtime = self.runtime.upgrade()?;
        let mut state = runtime.0.lock().unwrap_or_else(|e| e.into_inner());
        if !state.open || !state.owner_present {
            return None;
        }
        let registration = state.channels.get_mut(&self.channel)?;
        if registration.retired || !Arc::ptr_eq(&registration.token, &self.token) {
            return None;
        }
        Some(hand_off(registration))
    }

    fn transition(&self, runtime: &Runtime, change: impl FnOnce(&mut Registration)) -> bool {
        if !std::ptr::eq(self.runtime.as_ptr(), runtime) {
            return false;
        }
        self.with_current(change).is_some()
    }

    pub(super) fn incarnation(&self) -> Option<Arc<Incarnation>> {
        self.with_current(|registration| registration.incarnation.clone())
    }

    pub(super) fn admit<R>(
        &self,
        incarnation: &Incarnation,
        hand_off: impl FnOnce() -> R,
    ) -> Option<R> {
        self.with_current(|registration| {
            std::ptr::eq(incarnation, registration.incarnation.as_ref()).then(hand_off)
        })
        .flatten()
    }

    pub(super) fn channel(&self) -> u64 {
        self.channel
    }

    /// Create and retain each child under the fence, before its async body can be polled.
    pub(super) fn spawn_child(
        &self,
        work: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> bool {
        self.with_current(|registration| {
            registration.children.retain(|child| !child.is_finished());
            registration.children.push(tokio::spawn(work));
        })
        .is_some()
    }
}

#[path = "lifecycle_tests.rs"]
mod tests;
