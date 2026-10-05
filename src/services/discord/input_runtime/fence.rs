//! Memory-only admission and canonical population-lock capabilities.
use crate::services::provider::ProviderKind;
use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    LegacyOpen,
    Closing,
    Frozen,
    LedgerOpen,
    Handback,
    Held,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    Mode(Mode),
    LockTimeout,
    Persistence,
    StalePermit,
    Busy,
    ActorUnreachable,
}
struct State {
    mode: Mode,
    epoch: u64,
    effects: usize,
}
pub(crate) struct Gate {
    channel: u64,
    provider: ProviderKind,
    state: Mutex<State>,
    drained: Notify,
    protected: std::sync::atomic::AtomicBool,
    health: Mutex<Option<String>>,
}
struct Registration {
    gate: Arc<Gate>,
    next: OnceLock<Box<Registration>>,
}
static GATES: OnceLock<Box<Registration>> = OnceLock::new();
pub(crate) const WRITE_LOCK_DEADLINE: Duration = Duration::from_secs(3);

pub(crate) fn population_root() -> Option<PathBuf> {
    super::super::runtime_store::runtime_root()
}

pub(crate) fn lookup(provider: &ProviderKind, channel: u64) -> Option<Arc<Gate>> {
    find(|gate| gate.channel == channel && &gate.provider == provider)
}
pub(crate) fn channel_gate(channel: u64) -> Option<Arc<Gate>> {
    find(|gate| gate.channel == channel)
}
// Observability only, never admission or durable ownership evidence.
pub(crate) fn record_failure(
    provider: &ProviderKind,
    channel: u64,
    sources: &[u64],
    failure: Failure,
) {
    if let Some(gate) = lookup(provider, channel) {
        gate.record_failure(sources, failure);
    }
}
pub(crate) fn health_reasons() -> Vec<String> {
    let mut reasons = Vec::new();
    let mut slot = &GATES;
    while let Some(entry) = slot.get() {
        if entry
            .gate
            .protected
            .load(std::sync::atomic::Ordering::Acquire)
            && let Some(reason) = entry
                .gate
                .health
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        {
            reasons.push(reason);
        }
        slot = &entry.next;
    }
    reasons
}
fn find(matches: impl Fn(&Gate) -> bool) -> Option<Arc<Gate>> {
    let mut slot = &GATES;
    while let Some(entry) = slot.get() {
        if matches(&entry.gate)
            && entry
                .gate
                .protected
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return Some(entry.gate.clone());
        }
        slot = &entry.next;
    }
    None
}
impl Gate {
    pub(crate) fn record_failure(&self, sources: &[u64], failure: Failure) {
        *self.health.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!(
            "input_fence channel={} sources={sources:?} reason={failure:?}",
            self.channel
        ));
    }
    #[cfg(test)]
    pub(crate) fn clear_failure_for_test(&self) {
        *self.health.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    // Registration is dormant until all population writers consume the capability.
    pub(crate) fn protect(provider: ProviderKind, channel: u64) -> Result<Arc<Self>, Failure> {
        let mut slot = &GATES;
        loop {
            let entry = slot.get_or_init(|| {
                Box::new(Registration {
                    gate: Arc::new(Self {
                        channel,
                        provider: provider.clone(),
                        state: Mutex::new(State {
                            mode: Mode::LegacyOpen,
                            epoch: 1,
                            effects: 0,
                        }),
                        drained: Notify::new(),
                        protected: std::sync::atomic::AtomicBool::new(true),
                        health: Mutex::new(None),
                    }),
                    next: OnceLock::new(),
                })
            });
            if entry.gate.channel == channel && entry.gate.provider == provider {
                return if entry
                    .gate
                    .protected
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    Ok(entry.gate.clone())
                } else {
                    Err(Failure::StalePermit)
                };
            }
            slot = &entry.next;
        }
    }
    pub(crate) fn restore_protection(&self) -> Result<(), Failure> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.effects != 0 || self.protected.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Failure::Busy);
        }
        state.epoch += 1;
        state.mode = Mode::LegacyOpen;
        *self.health.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.protected
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }
    pub(crate) fn mode(&self) -> Mode {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).mode
    }
    pub(crate) fn admit(self: &Arc<Self>) -> Result<Permit, Failure> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !self.protected.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Failure::StalePermit);
        }
        if state.mode != Mode::LegacyOpen {
            return Err(Failure::Mode(state.mode));
        }
        state.effects += 1;
        Ok(Permit {
            gate: self.clone(),
            epoch: state.epoch,
        })
    }
    pub(crate) fn close(self: &Arc<Self>) -> Result<Closing, Failure> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !self.protected.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Failure::StalePermit);
        }
        if state.mode != Mode::LegacyOpen {
            return Err(Failure::Mode(state.mode));
        }
        state.mode = Mode::Closing;
        Ok(Closing {
            gate: self.clone(),
            epoch: std::sync::atomic::AtomicU64::new(state.epoch),
        })
    }
}
pub(crate) struct Permit {
    gate: Arc<Gate>,
    epoch: u64,
}
impl Permit {
    pub(crate) fn validate(&self, provider: &ProviderKind, channel: u64) -> Result<(), Failure> {
        let state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        if channel != self.gate.channel
            || provider != &self.gate.provider
            || self.epoch != state.epoch
        {
            return Err(Failure::StalePermit);
        }
        if !matches!(state.mode, Mode::LegacyOpen | Mode::Closing) {
            return Err(Failure::Mode(state.mode));
        }
        Ok(())
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        state.effects -= 1;
        if state.effects == 0 {
            self.gate.drained.notify_waiters();
        }
    }
}
pub(crate) struct Closing {
    gate: Arc<Gate>,
    epoch: std::sync::atomic::AtomicU64,
}
impl Closing {
    pub(crate) async fn drain(&self) {
        loop {
            let notified = self.gate.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self
                .gate
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .effects
                == 0
            {
                return;
            }
            notified.await;
        }
    }
    pub(crate) fn channel(&self) -> u64 {
        self.gate.channel
    }
    pub(crate) fn provider(&self) -> &ProviderKind {
        &self.gate.provider
    }
    // This port is for the supervisor after durable handback, not cancellation cleanup.
    pub(crate) fn release_protection_after_handback(&self) -> Result<(), Failure> {
        let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.effects != 0
            || state.epoch != self.epoch.load(std::sync::atomic::Ordering::Acquire)
            || state.mode != Mode::Handback
        {
            return Err(Failure::Busy);
        }
        state.epoch += 1;
        state.mode = Mode::LegacyOpen;
        self.gate
            .protected
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }
    pub(crate) fn freeze(
        &self,
        ack: crate::services::turn_orchestrator::input_fence::FreezeAck,
    ) -> Result<(), Failure> {
        let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.mode != Mode::Closing
            || state.effects != 0
            || state.epoch != self.epoch.load(std::sync::atomic::Ordering::Acquire)
            || ack.channel() != self.channel()
            || ack.provider() != self.provider()
            || !ack.matches(self)
        {
            return Err(Failure::Busy);
        }
        state.mode = Mode::Frozen;
        state.epoch += 1;
        self.epoch
            .store(state.epoch, std::sync::atomic::Ordering::Release);
        Ok(())
    }
    pub(crate) fn frozen_population(&self, root: &Path) -> Result<ClosedPopulationGuard, Failure> {
        if self.gate.mode() != Mode::Frozen {
            return Err(Failure::Busy);
        }
        let guard = self.population(root)?;
        if self.gate.mode() != Mode::Frozen {
            return Err(Failure::Busy);
        }
        Ok(guard)
    }
    #[cfg(test)]
    pub(crate) fn frozen_for_test(provider: ProviderKind, channel: u64) -> Arc<Self> {
        Arc::new(Self {
            gate: Arc::new(Gate {
                provider,
                channel,
                state: Mutex::new(State {
                    mode: Mode::Frozen,
                    epoch: 1,
                    effects: 0,
                }),
                drained: Notify::new(),
                protected: std::sync::atomic::AtomicBool::new(true),
                health: Mutex::new(None),
            }),
            epoch: std::sync::atomic::AtomicU64::new(1),
        })
    }
    pub(crate) fn population(&self, root: &Path) -> Result<ClosedPopulationGuard, Failure> {
        if scope_held() {
            return Err(Failure::Busy);
        }
        let state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.effects != 0
            || state.epoch != self.epoch.load(std::sync::atomic::Ordering::Acquire)
            || !self
                .gate
                .protected
                .load(std::sync::atomic::Ordering::Acquire)
            || !matches!(state.mode, Mode::Closing | Mode::Frozen | Mode::Handback)
        {
            return Err(Failure::Busy);
        }
        drop(state);
        PopulationGuard::try_acquire(root, self.provider(), self.channel())
            .map(ClosedPopulationGuard)
    }
}
// A worker capability is installed only around synchronous blocking-task work.
thread_local! { static BLOCKING_WORK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
pub(crate) fn blocking<T>(work: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            BLOCKING_WORK.with(|held| held.set(self.0));
        }
    }
    let _reset = Reset(BLOCKING_WORK.with(|held| held.replace(true)));
    work()
}
pub(crate) fn require_worker() -> Result<(), Failure> {
    if BLOCKING_WORK.with(std::cell::Cell::get) || tokio::runtime::Handle::try_current().is_err() {
        Ok(())
    } else {
        Err(Failure::Busy)
    }
}
thread_local! { static INFLIGHT_WRITER: RefCell<std::sync::Weak<()>> = const { RefCell::new(std::sync::Weak::new()) }; }
pub(crate) fn scope_held() -> bool {
    POPULATION.with(|held| held.borrow().is_some())
        || INFLIGHT_WRITER.with(|held| held.borrow().upgrade().is_some())
}
pub(crate) struct ClosedPopulationGuard(PopulationGuard);
impl ClosedPopulationGuard {
    pub(crate) fn borrowed<T>(
        &self,
        root: &Path,
        provider: &ProviderKind,
        channel: u64,
        work: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        self.0.borrowed(root, provider, channel, work)
    }
}
pub(crate) struct PopulationWriter {
    _guard: PopulationGuard,
    _scope: Arc<()>,
    _permit: Permit,
}
pub(crate) fn inflight_writer(path: &Path) -> Result<Option<PopulationWriter>, Failure> {
    let Some(provider_dir) = path.parent() else {
        return Ok(None);
    };
    let channel = path
        .file_stem()
        .and_then(|n| n.to_str())
        .and_then(|n| n.parse().ok());
    let Some(gate) = find(|gate| {
        Some(gate.channel) == channel
            && provider_dir.file_name().and_then(|n| n.to_str()) == Some(gate.provider.as_str())
    }) else {
        return Ok(None);
    };
    let failed = |failure| {
        gate.record_failure(&[], failure);
        failure
    };
    if scope_held() {
        return Err(failed(Failure::Busy));
    }
    let permit = gate.admit().map_err(failed)?;
    require_worker().map_err(failed)?;
    let inflight = provider_dir
        .parent()
        .ok_or_else(|| failed(Failure::Persistence))?;
    if inflight.file_name().and_then(|n| n.to_str()) != Some("discord_inflight") {
        return Err(failed(Failure::StalePermit));
    }
    let root = inflight
        .parent()
        .ok_or_else(|| failed(Failure::Persistence))?;
    let guard =
        PopulationGuard::writer(root, &gate.provider, gate.channel, &permit).map_err(failed)?;
    // Weak origin-thread marker expires even when an owned guard is dropped on another worker.
    let scope = Arc::new(());
    INFLIGHT_WRITER.with(|held| *held.borrow_mut() = Arc::downgrade(&scope));
    Ok(Some(PopulationWriter {
        _guard: guard,
        _scope: scope,
        _permit: permit,
    }))
}
pub(crate) struct PopulationGuard {
    _file: std::fs::File,
    root: PathBuf,
    provider: String,
    channel: u64,
}
impl PopulationGuard {
    fn open(root: &Path, provider: &ProviderKind, channel: u64) -> Result<Self, Failure> {
        let path = root
            .join("discord_inflight")
            .join(provider.as_str())
            .join(format!("{channel}.json.lock"));
        std::fs::create_dir_all(path.parent().ok_or(Failure::Persistence)?)
            .map_err(|_| Failure::Persistence)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|_| Failure::Persistence)?;
        Ok(Self {
            _file: file,
            root: root.to_owned(),
            provider: provider.as_str().to_owned(),
            channel,
        })
    }
    fn try_acquire(root: &Path, provider: &ProviderKind, channel: u64) -> Result<Self, Failure> {
        let guard = Self::open(root, provider, channel)?;
        guard._file.try_lock().map_err(|e| match e {
            std::fs::TryLockError::WouldBlock => Failure::Busy,
            _ => Failure::Persistence,
        })?;
        Ok(guard)
    }
    fn writer(
        root: &Path,
        provider: &ProviderKind,
        channel: u64,
        permit: &Permit,
    ) -> Result<Self, Failure> {
        Self::writer_wait(root, provider, channel, permit, Instant::now, || {
            std::thread::park_timeout(Duration::from_millis(5))
        })
    }
    fn writer_wait(
        root: &Path,
        provider: &ProviderKind,
        channel: u64,
        permit: &Permit,
        mut now: impl FnMut() -> Instant,
        mut wait: impl FnMut(),
    ) -> Result<Self, Failure> {
        permit.validate(provider, channel)?;
        let guard = Self::open(root, provider, channel)?;
        let deadline = now() + WRITE_LOCK_DEADLINE;
        loop {
            match guard._file.try_lock() {
                Ok(()) => {
                    permit.validate(provider, channel)?;
                    return Ok(guard);
                }
                Err(std::fs::TryLockError::WouldBlock) if now() < deadline => {
                    #[cfg(test)]
                    WAIT_OBSERVER.with(|hook| {
                        if let Some(hook) = hook.borrow_mut().take() {
                            hook();
                        }
                    });
                    #[cfg(test)]
                    if let Some(hook) = ACTOR_WAIT
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&channel)
                    {
                        hook();
                    }
                    wait();
                }
                Err(std::fs::TryLockError::WouldBlock) => return Err(Failure::LockTimeout),
                Err(_) => return Err(Failure::Persistence),
            }
        }
    }
    pub(crate) fn matches(&self, root: &Path, provider: &ProviderKind, channel: u64) -> bool {
        self.root == root && self.provider == provider.as_str() && self.channel == channel
    }
    pub(crate) fn borrowed<T>(
        &self,
        root: &Path,
        provider: &ProviderKind,
        channel: u64,
        work: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<T> {
        if !self.matches(root, provider, channel) {
            return Err(io::Error::other("population guard identity mismatch"));
        }
        work()
    }
}
impl Drop for PopulationGuard {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

thread_local! { static POPULATION: RefCell<Option<PopulationGuard>> = const { RefCell::new(None) }; }
pub(crate) struct PopulationScope(std::marker::PhantomData<std::rc::Rc<()>>);
impl PopulationScope {
    pub(crate) fn hold(guard: ClosedPopulationGuard) -> Self {
        Self::install(guard.0)
    }
    pub(crate) fn writer(
        root: &Path,
        provider: &ProviderKind,
        channel: u64,
        permit: &Permit,
    ) -> Result<Self, Failure> {
        if scope_held() {
            return Err(Failure::Busy);
        }
        PopulationGuard::writer(root, provider, channel, permit).map(Self::install)
    }
    fn install(guard: PopulationGuard) -> Self {
        POPULATION.with(|held| {
            assert!(held.borrow().is_none(), "nested population scope");
            *held.borrow_mut() = Some(guard);
        });
        Self(std::marker::PhantomData)
    }
}
impl Drop for PopulationScope {
    fn drop(&mut self) {
        POPULATION.with(|held| held.borrow_mut().take());
    }
}
pub(crate) fn any_protected() -> bool {
    find(|_| true).is_some()
}
pub(crate) fn write<T>(
    provider: &ProviderKind,
    channel: u64,
    work: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let Some(gate) = lookup(provider, channel) else {
        return work();
    };
    let failed = |failure| {
        gate.record_failure(&[], failure);
        format!("input fence: {failure:?}")
    };
    let root = population_root().ok_or_else(|| failed(Failure::Persistence))?;
    if POPULATION.with(|held| {
        held.borrow()
            .as_ref()
            .is_some_and(|guard| guard.matches(&root, provider, channel))
    }) {
        return work().inspect_err(|_| gate.record_failure(&[], Failure::Persistence));
    }
    if scope_held() {
        return Err(failed(Failure::Busy));
    }
    let permit = gate.admit().map_err(failed)?;
    require_worker().map_err(failed)?;
    let _scope = PopulationScope::writer(&root, provider, channel, &permit).map_err(failed)?;
    work().inspect_err(|_| gate.record_failure(&[], Failure::Persistence))
}

#[cfg(test)]
thread_local! { static WAIT_OBSERVER: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) }; }

#[cfg(test)]
#[path = "fence_tests.rs"]
mod tests;

#[cfg(test)]
static ACTOR_WAIT: std::sync::LazyLock<
    Mutex<std::collections::HashMap<u64, Box<dyn FnOnce() + Send>>>,
> = std::sync::LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));
#[cfg(test)]
pub(crate) fn install_wait_observer(channel: u64, hook: Box<dyn FnOnce() + Send>) {
    ACTOR_WAIT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(channel, hook);
}
