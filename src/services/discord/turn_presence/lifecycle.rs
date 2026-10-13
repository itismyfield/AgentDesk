//! Dormant, process-local registration fences; no turn facts or restart authority live here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use tokio::task::{AbortHandle, JoinHandle};

use super::activity::Activity;
use super::admission::Incarnation;

/// The supervisor state a registration shows; it changes under the registration's fence.
pub(super) trait Shown: Send + Sync {
    /// The registration's approvals and children are gone; a retire also takes down its display.
    fn withdrawn(&self, channel: u64, retire: bool);
    /// A fresh reading seated a new approval, so the channel starts over from it.
    fn seated(&self, channel: u64);
}

pub(crate) struct Runtime(Mutex<State>);

struct State {
    open: bool,
    owner_present: bool,
    owner_token: Arc<()>,
    channels: HashMap<u64, Registration>,
    shown: Option<Arc<dyn Shown>>,
}

impl State {
    fn fresh(&mut self) {
        for registration in self.channels.values_mut().filter(|r| !r.retired) {
            registration.reset(false);
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
    /// Survives adoption advances; only a reset or retire starts a new generation.
    generation: Arc<()>,
    pub(super) incarnation: Arc<Incarnation>,
    pub(super) activity: Activity,
    pub(super) deadline: Option<Instant>,
    children: Vec<JoinHandle<()>>,
    retired: bool,
    /// Kept across resets, so every later reset still reaches the same display.
    shown: Option<(Arc<dyn Shown>, u64)>,
}

impl Default for Registration {
    fn default() -> Self {
        Self {
            token: Arc::new(()),
            generation: Arc::new(()),
            incarnation: Arc::default(),
            activity: Activity::Unknown,
            deadline: None,
            children: Vec::new(),
            retired: false,
            shown: None,
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
        self.withdrawn(false);
    }

    pub(super) fn seated(&self) {
        if let Some((shown, channel)) = &self.shown {
            shown.seated(*channel);
        }
    }

    fn withdrawn(&self, retire: bool) {
        if let Some((shown, channel)) = &self.shown {
            shown.withdrawn(*channel, retire);
        }
    }

    fn reset(&mut self, retire: bool) {
        self.incarnation.invalidate();
        let shown = self.shown.take();
        *self = Self::default();
        (self.retired, self.shown) = (retire, shown);
        self.withdrawn(retire);
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
        Self::showing(None)
    }
}

impl Runtime {
    /// A runtime whose resets clear `shown` under the same lock that revokes the approval.
    pub(super) fn showing(shown: Option<Arc<dyn Shown>>) -> Self {
        Self(Mutex::new(State {
            open: true,
            owner_present: false,
            owner_token: Arc::new(()),
            channels: HashMap::new(),
            shown,
        }))
    }

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
        let shown = state.shown.clone().map(|shown| (shown, channel));
        let registration = state.channels.entry(channel).or_default();
        if registration.retired {
            *registration = Registration::default();
        }
        registration.shown = registration.shown.take().or(shown);
        Some(Ticket {
            runtime: Arc::downgrade(self),
            channel,
            token: registration.token.clone(),
        })
    }

    pub(crate) fn invalidate_if_current(&self, ticket: &Ticket, cause: &'static str) -> bool {
        ticket.transition(self, |registration| {
            registration.reset(false);
            tracing::debug!(
                channel_id = ticket.channel,
                cause,
                "presence approval invalidated"
            );
        })
    }

    pub(crate) fn retire_if_current(&self, ticket: &Ticket) -> bool {
        ticket.transition(self, |registration| registration.reset(true))
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

    /// The ticket a registration holds after its token advanced under this one.
    pub(super) fn successor(&self, registration: &Registration) -> Self {
        Self {
            runtime: self.runtime.clone(),
            channel: self.channel,
            token: registration.token.clone(),
        }
    }

    /// Create and retain each child under the fence, before its async body can be polled.
    pub(super) fn spawn_child(
        &self,
        work: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> bool {
        self.own_child(work, drop)
    }

    /// [`Self::spawn_child`], handing its abort handle to `arm` while the fence still holds it.
    pub(super) fn own_child(
        &self,
        work: impl std::future::Future<Output = ()> + Send + 'static,
        arm: impl FnOnce(AbortHandle),
    ) -> bool {
        self.with_current(|registration| {
            registration.children.retain(|child| !child.is_finished());
            let child = tokio::spawn(work);
            arm(child.abort_handle());
            registration.children.push(child);
        })
        .is_some()
    }
}

impl super::entrypoints::Fence for Runtime {
    fn withdraw(&self, channel: u64, retire: bool, cause: &'static str) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some(registration) = state.channels.get_mut(&channel).filter(|r| !r.retired) else {
            return;
        };
        registration.reset(retire);
        tracing::debug!(
            channel_id = channel,
            cause,
            retire,
            "presence approval withdrawn"
        );
    }

    fn withdraw_gateway(
        &self,
        gate: &Arc<crate::services::tui_o::ownership::OwnershipGate>,
        cause: &'static str,
    ) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        for (channel, registration) in state.channels.iter_mut() {
            if !registration.retired && registration.incarnation.rests_on(gate) {
                registration.reset(false);
                tracing::debug!(
                    channel_id = channel,
                    cause,
                    "presence gateway approval withdrawn"
                );
            }
        }
    }

    fn withdraw_unmounted(
        &self,
        channel: u64,
        mount: Option<&Arc<super::entrypoints::HomeGate>>,
        cause: &'static str,
    ) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        // A late notice names a mount the registry has since moved past; only the current one counts.
        let current = crate::services::cluster::channel_home::registered_channel(channel);
        if mount.map(Arc::as_ptr) != current.as_ref().map(Arc::as_ptr) {
            return;
        }
        let Some(registration) = state.channels.get_mut(&channel) else {
            return;
        };
        if !registration.retired && registration.incarnation.rests_off(mount) {
            registration.reset(false);
            tracing::debug!(
                channel_id = channel,
                cause,
                "presence approval withdrawn on home change"
            );
        }
    }

    fn generation(&self, channel: u64) -> Option<Arc<()>> {
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let registration = state.channels.get(&channel).filter(|r| !r.retired)?;
        Some(registration.generation.clone())
    }

    fn withdraw_generation(&self, channel: u64, generation: &Arc<()>, cause: &'static str) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some(registration) = state.channels.get_mut(&channel) else {
            return;
        };
        if !registration.retired && Arc::ptr_eq(&registration.generation, generation) {
            registration.reset(false);
            tracing::debug!(channel_id = channel, cause, "presence lease withdrawn");
        }
    }

    fn suspend(&self) {
        self.suspend_runtime();
    }

    fn resume(&self) {
        self.resume_fresh();
    }
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
