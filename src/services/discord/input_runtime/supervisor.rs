//! Per-channel input supervisor: one registration and ledger slot per channel, ledger loans, and
//! the boot order up to input admission. No production path starts it yet.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{OwnedMutexGuard, watch};

use super::clear::{self, ClearHost, Step, Unresolved};
use super::command::{ScanCommit, SupervisorCmd};
use super::fence::{self, Closing, Failure, Gate, Mode};
use super::ordering::AdmissionOrder;
use super::receipt::{self, Deferred, Receipt};
use super::reconcile::{self, HoldCause};
use crate::services::provider::ProviderKind;
use crate::services::tui_input::ledger::{Ledger, LedgerLease, LedgerSlot, Presence};
use crate::services::tui_input::transition::{self, Host, Move, Outcome};
use crate::services::tui_o::writer::binding::{BindingEvent, BindingEvents};

pub(crate) mod drive;

/// Retries per boot for a held stage; an exhausted stage stays held until the next boot.
pub(crate) const BUDGET: u32 = 8;
pub(crate) const DRAIN_LIMIT: Duration = Duration::from_secs(30);

pub(crate) static REGISTRY: Registry = Registry::new();

pub(crate) struct Registry {
    used: AtomicBool,
    entries: Mutex<BTreeMap<(String, u64), Entry>>,
    // A close outlives its supervisor, so a later one in this process continues from it.
    closings: Mutex<BTreeMap<(String, u64), Arc<Closing>>>,
    #[cfg(test)]
    pub(crate) on_release: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

#[derive(Default)]
struct Entry {
    poisoned: bool,
    health: BTreeMap<&'static str, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    Duplicate,
    Poisoned,
}

impl Registry {
    pub(crate) const fn new() -> Self {
        Self {
            used: AtomicBool::new(false),
            entries: Mutex::new(BTreeMap::new()),
            closings: Mutex::new(BTreeMap::new()),
            #[cfg(test)]
            on_release: Mutex::new(None),
        }
    }

    /// False until the first registration, so an unused registry costs health nothing.
    pub(crate) fn used(&self) -> bool {
        self.used.load(Ordering::Acquire)
    }

    pub(crate) fn register(
        &'static self,
        provider: &ProviderKind,
        channel: u64,
        root: &Path,
    ) -> Result<Registration, Refused> {
        let key = (provider.as_str().to_owned(), channel);
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = entries.get(&key) {
            return Err(match entry.poisoned {
                true => Refused::Poisoned,
                false => Refused::Duplicate,
            });
        }
        entries.insert(key.clone(), Entry::default());
        self.used.store(true, Ordering::Release);
        Ok(Registration {
            registry: self,
            key,
            slot: Some(LedgerSlot::new(root, channel)),
            released: false,
        })
    }

    pub(crate) fn health_reasons(&self) -> Vec<String> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries
            .values()
            .flat_map(|entry| entry.health.values().cloned())
            .collect()
    }
}

/// A channel's one live supervisor. Dropping it without `release` poisons the channel for this
/// process, because a lent handle may still be writing.
pub(crate) struct Registration {
    registry: &'static Registry,
    key: (String, u64),
    slot: Option<LedgerSlot>,
    released: bool,
}

impl Registration {
    pub(crate) fn slot(&mut self) -> Option<LedgerSlot> {
        self.slot.take()
    }

    fn closing(&self, gate: &Arc<Gate>) -> Result<Arc<Closing>, Failure> {
        let mut closings = (self.registry.closings.lock()).unwrap_or_else(|e| e.into_inner());
        if let Some(closing) = closings.get(&self.key) {
            return Ok(closing.clone());
        }
        let closing = Arc::new(gate.close()?);
        closings.insert(self.key.clone(), closing.clone());
        Ok(closing)
    }

    // The gate reopened to Legacy, so the next supervisor must close it afresh.
    fn reopened(&self, closing: &Closing, history: bool) -> Result<(), Failure> {
        closing.abort_close(history)?;
        let mut closings = (self.registry.closings.lock()).unwrap_or_else(|e| e.into_inner());
        closings.remove(&self.key);
        Ok(())
    }

    pub(crate) fn report(&self, cause: &HoldCause, held: bool) {
        let mut entries = (self.registry.entries.lock()).unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = entries.get_mut(&self.key).filter(|e| !e.poisoned) {
            match held {
                true => entry
                    .health
                    .insert(cause.slot(), cause.health(&self.key.0, self.key.1)),
                false => entry.health.remove(cause.slot()),
            };
        }
    }

    /// Closes the slot's handle before freeing the channel; a lent slot is never released.
    pub(crate) fn release(mut self, slot: LedgerSlot) -> bool {
        if slot.loaned() {
            return false;
        }
        drop(slot);
        let mut entries = (self.registry.entries.lock()).unwrap_or_else(|e| e.into_inner());
        entries.remove(&self.key);
        drop(entries);
        #[cfg(test)]
        let observer = (self.registry.on_release.lock())
            .unwrap_or_else(|e| e.into_inner())
            .take();
        #[cfg(test)]
        if let Some(observe) = observer {
            observe();
        }
        self.released = true;
        true
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let lost = HoldCause::TransitionHeld("supervisor_lost");
        let mut entries = (self.registry.entries.lock()).unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = entries.get_mut(&self.key) {
            entry.poisoned = true;
            entry.health = BTreeMap::from([(lost.slot(), lost.health(&self.key.0, self.key.1))]);
        }
    }
}

/// The last binding seq applied. A watch value is only a hint; events are always reread from the
/// log after the cursor, in seq order.
pub(crate) struct Cursor {
    binding: Arc<dyn BindingEvents>,
    channel: u64,
    rx: watch::Receiver<u64>,
    seq: u64,
}

impl Cursor {
    /// Subscribes before the baseline read, so an append in between is read once and its wake
    /// applies nothing.
    pub(crate) fn start(
        binding: Arc<dyn BindingEvents>,
        channel: u64,
    ) -> Result<(Self, Vec<BindingEvent>), String> {
        let rx = binding.subscribe(channel);
        let mut cursor = Self {
            binding,
            channel,
            rx,
            seq: 0,
        };
        let events = cursor.read()?;
        Ok((cursor, events))
    }

    pub(crate) fn seq(&self) -> u64 {
        self.seq
    }

    fn read(&mut self) -> Result<Vec<BindingEvent>, String> {
        let events = (self.binding).binding_events_since(self.channel, self.seq)?;
        if (events.iter().zip(self.seq + 1..)).any(|(event, seq)| event.seq != seq) {
            return Err(format!("binding seq gap after {}", self.seq));
        }
        self.seq = events.last().map_or(self.seq, |event| event.seq);
        Ok(events)
    }

    /// Events past the cursor once the log changes; a wake at or below the cursor applies none.
    pub(crate) async fn wake(&mut self) -> Result<Vec<BindingEvent>, String> {
        (self.rx.changed().await).map_err(|_| "binding watch closed".to_owned())?;
        if *self.rx.borrow_and_update() <= self.seq {
            return Ok(Vec::new());
        }
        self.read()
    }

    /// Resubscribes after a failed watch, then reads whatever the log gained meanwhile.
    pub(crate) fn recover(&mut self) -> Result<Vec<BindingEvent>, String> {
        self.rx = self.binding.subscribe(self.channel);
        self.read()
    }
}

/// Runs blocking ledger work and awaits it to the end; the slot stays lent meanwhile. A panicked
/// worker has dropped its lease before the join completes.
pub(crate) async fn loan<T: Send + 'static>(
    slot: &mut LedgerSlot,
    work: impl FnOnce(&mut LedgerLease) -> T + Send + 'static,
) -> Option<T> {
    let mut lease = slot.lend().ok()?;
    let joined = tokio::task::spawn_blocking(move || {
        let out = fence::blocking(|| work(&mut lease));
        (out, lease)
    })
    .await;
    match joined {
        Ok((out, lease)) => {
            slot.restore(lease);
            Some(out)
        }
        Err(_) => {
            slot.restore_fresh();
            None
        }
    }
}

/// Lends the handle to a clear resume and awaits the worker's reply to the end; the reply is lost
/// only after the worker's last ledger write or during its unwinding.
pub(crate) async fn clear_loan<H: ClearHost>(
    slot: &mut LedgerSlot,
    host: H,
    guard: OwnedMutexGuard<()>,
) -> Option<clear::Outcome> {
    let ledger = match slot.lend().ok()?.into_ledger() {
        Ok(ledger) => ledger,
        Err(_) => {
            slot.restore_fresh();
            return None;
        }
    };
    match clear::resume_blocking(ledger, host, guard).await {
        Ok((ledger, outcome)) => {
            slot.restore_ledger(ledger);
            Some(outcome)
        }
        Err(_) => {
            slot.restore_fresh();
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Request {
    Ledger,
    Legacy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Landing {
    Admitted,
    Legacy,
    HandedBack,
    Held(HoldCause),
}

/// The adapters a supervisor drives; production builds them from the gateway runtime.
pub(crate) trait Ports: Send + 'static {
    type Clear: ClearHost;
    type Move: Host + Send + 'static;
    /// Freezes the drained channel's Legacy queue.
    fn freeze(&mut self, closing: Arc<Closing>) -> Step<'_, Result<(), Failure>>;
    /// A clear host with the session transition guard its resume runs under.
    fn clear(&mut self) -> Step<'_, Option<(Self::Clear, OwnedMutexGuard<()>)>>;
    fn transition(&mut self, closing: Arc<Closing>) -> io::Result<Self::Move>;
    fn take_notices(host: &mut Self::Move) -> Vec<(Option<u64>, &'static str)>;
    /// False when delivery failed, so the same Notice may be sent again later.
    fn notice(&mut self, text: String) -> Step<'_, bool>;
}

pub(crate) struct Config {
    pub provider: ProviderKind,
    pub channel: u64,
    pub root: PathBuf,
    pub request: Request,
    /// Why this channel may not use the ledger at all, such as a non-tmux session host.
    pub refusal: Option<&'static str>,
    pub binding: Arc<dyn BindingEvents>,
}

pub(crate) struct Supervisor<P: Ports> {
    config: Config,
    ports: P,
    registration: Registration,
    slot: LedgerSlot,
    cursor: Option<Cursor>,
    watch_lost: bool,
    host: Option<P::Move>,
    movement: Option<Move>,
    sent: BTreeSet<(Option<u64>, &'static str)>,
    admitted: bool,
    order: AdmissionOrder,
    admission_epoch: u64,
    #[cfg(test)]
    pub(crate) handbacks: usize,
}

fn held(reason: &'static str) -> HoldCause {
    HoldCause::TransitionHeld(reason)
}

impl<P: Ports> Supervisor<P> {
    /// S0: refuses a channel that already has a live or lost supervisor in this process.
    pub(crate) fn start(
        registry: &'static Registry,
        config: Config,
        ports: P,
    ) -> Result<Self, Refused> {
        let mut registration = registry.register(&config.provider, config.channel, &config.root)?;
        let slot = registration.slot().ok_or(Refused::Duplicate)?;
        let order = AdmissionOrder::new(config.provider.clone(), config.channel, 1, 0);
        Ok(Self {
            config,
            ports,
            registration,
            slot,
            cursor: None,
            watch_lost: false,
            host: None,
            movement: None,
            sent: BTreeSet::new(),
            admitted: false,
            order,
            admission_epoch: 1,
            #[cfg(test)]
            handbacks: 0,
        })
    }

    /// The ledger owns the channel and no clear, loan or close is outstanding.
    pub(crate) fn admission_open(&self) -> bool {
        self.admitted && !self.slot.loaned()
    }

    pub(crate) fn cursor(&mut self) -> Option<&mut Cursor> {
        self.cursor.as_mut()
    }

    pub(crate) fn slot(&mut self) -> &mut LedgerSlot {
        &mut self.slot
    }

    pub(crate) fn release(self) -> bool {
        self.registration.release(self.slot)
    }

    /// S1 to S7. A holding stage reports health and leaves every later stage unrun.
    pub(crate) async fn boot(&mut self) -> Landing {
        // Each boot must pass every stage again, so an earlier admission never outlives it.
        self.admitted = false;
        self.invalidate_order();
        match self.stages().await {
            Ok(landing) => landing,
            Err(cause) => {
                self.registration.report(&cause, true);
                Landing::Held(cause)
            }
        }
    }

    fn invalidate_order(&mut self) {
        match self.admission_epoch.checked_add(1) {
            Some(epoch) => {
                self.admission_epoch = epoch;
                self.order.invalidate(epoch);
            }
            None => self.admitted = false,
        }
    }

    /// All receipt work borrows the existing slot; readiness for Enter is a separate decision.
    async fn input_command(&mut self, command: SupervisorCmd, receipt_open: bool) {
        use crate::services::tui_input::receipt_identity::Responsibility;
        match command {
            SupervisorCmd::Clear => {}
            SupervisorCmd::PendingSource { sources, reply } => {
                let _ = reply.send(self.order.pending(&sources));
            }
            SupervisorCmd::LookupResponsibility { identity, reply } => {
                let result = if identity.execution_channel_id != self.config.channel {
                    Responsibility::Conflict
                } else {
                    loan(&mut self.slot, move |lease| {
                        lease
                            .get()
                            .and_then(|ledger| ledger.rows())
                            .map(|rows| rows.responsibility(&identity))
                            .unwrap_or(Responsibility::Unknown)
                    })
                    .await
                    .unwrap_or(Responsibility::Unknown)
                };
                let _ = reply.send(result);
            }
            SupervisorCmd::BeginScan {
                sources,
                horizon,
                complete_fetch,
                reply,
            } => {
                let result =
                    self.order
                        .begin_scan(self.admission_epoch, sources, horizon, complete_fetch);
                let _ = reply.send(result);
            }
            SupervisorCmd::CommitFromScan {
                source,
                mut capability,
                reply,
            } => {
                let key = source.key();
                let result = if !receipt_open || !self.admission_open() {
                    Receipt::Deferred(Deferred::Closed)
                } else if source.identity().execution_channel_id != self.config.channel
                    || self
                        .order
                        .permits(&capability, self.admission_epoch, key)
                        .is_err()
                {
                    Receipt::Deferred(Deferred::Order)
                } else {
                    loan(&mut self.slot, move |lease| receipt::commit(lease, *source))
                        .await
                        .unwrap_or(Receipt::Deferred(Deferred::SupervisorLost))
                };
                match &result {
                    Receipt::Accepted(_) | Receipt::DuplicateQueued(_) => {
                        let _ = self
                            .order
                            .settle(&mut capability, self.admission_epoch, key);
                    }
                    Receipt::Deferred(_) => {
                        let _ = self.order.defer(&capability, self.admission_epoch, key);
                    }
                }
                let _ = reply.send(ScanCommit {
                    receipt: result,
                    capability,
                });
            }
            SupervisorCmd::SettleFromScan {
                source,
                disposition: _,
                mut capability,
                reply,
            } => {
                let result = self
                    .order
                    .settle(&mut capability, self.admission_epoch, source)
                    .map(|()| capability);
                let _ = reply.send(result);
            }
            SupervisorCmd::CompleteScan { capability, reply } => {
                let result = self.order.complete(capability, self.admission_epoch);
                let _ = reply.send(result);
            }
        }
    }

    async fn stages(&mut self) -> Result<Landing, HoldCause> {
        let (provider, channel) = (self.config.provider.clone(), self.config.channel);
        let binding = self.config.binding.clone();
        let (cursor, _) =
            Cursor::start(binding, channel).map_err(|_| HoldCause::BindingUnreadable)?;
        self.cursor = Some(cursor);
        // A cause leaves health only where its own check passed; other held causes stay.
        self.registration
            .report(&HoldCause::BindingUnreadable, false);
        let history = match Ledger::probe(&self.config.root, channel) {
            Presence::Unreadable => return Err(HoldCause::LedgerUnreadable),
            presence => presence == Presence::Present,
        };
        let gate = Gate::protect(provider, channel).map_err(|_| held("protect"))?;
        let closing = (self.registration.closing(&gate)).map_err(|_| held("close"))?;
        if let Some(reason) = self.config.refusal {
            self.registration
                .report(&HoldCause::ModeRefused(reason), true);
            // Without ledger history the channel never left Legacy, so it simply reopens.
            (self.registration.reopened(&closing, history))
                .map_err(|_| HoldCause::ModeRefused(reason))?;
            return Ok(Landing::Legacy);
        }
        let mut attempt = 0;
        while gate.mode() == Mode::Closing && closing.drain_within(DRAIN_LIMIT).await.is_err() {
            attempt += 1;
            if attempt == BUDGET {
                return Err(held("drain_timeout"));
            }
            tokio::time::sleep(transition::backoff(attempt - 1)).await;
        }
        if gate.mode() == Mode::Closing {
            (self.ports.freeze(closing.clone()).await).map_err(|_| held("freeze"))?;
        }
        let unbound = loan(&mut self.slot, |lease| {
            Ok::<_, io::Error>(lease.get()?.rows()?.unbound().clone())
        })
        .await
        .ok_or(held("supervisor_lost"))?
        .map_err(|_| HoldCause::LedgerUnreadable)?;
        self.registration
            .report(&HoldCause::LedgerUnreadable, false);
        if !unbound.is_empty() {
            let keys: Vec<u64> = unbound.into_iter().collect();
            self.registration
                .report(&HoldCause::Unbound(keys.clone()), true);
            for key in keys {
                self.notify(Some(key), "unbound").await;
            }
        }
        self.resume_clear().await?;
        self.transition(&closing, history).await
    }

    /// S5: settles a clear cutoff a crash left behind before anything else touches the ledger.
    async fn resume_clear(&mut self) -> Result<(), HoldCause> {
        for retry in 0..=BUDGET {
            let (host, guard) = (self.ports.clear().await).ok_or(held("clear_unavailable"))?;
            let outcome =
                (clear_loan(&mut self.slot, host, guard).await).ok_or(held("supervisor_lost"))?;
            let reset = match outcome {
                clear::Outcome::Cleared | clear::Outcome::Idle => return Ok(()),
                clear::Outcome::Held(Unresolved::ResetUnconfirmed) => true,
                clear::Outcome::Held(
                    Unresolved::PgUnavailable
                    | Unresolved::CommitUncertain
                    | Unresolved::ResolveUncertain,
                ) => false,
                _ => return Err(held("clear_held")),
            };
            if retry < BUDGET {
                self.pause(retry, reset).await;
            }
        }
        self.notify(None, "clear_retry_exhausted").await;
        Err(held("clear_retry_exhausted"))
    }

    /// Waits out one backoff. For an unconfirmed reset a wake that advances the cursor ends it
    /// early; a duplicate wake applies nothing and keeps waiting.
    async fn pause(&mut self, attempt: u32, reset: bool) {
        let sleep = tokio::time::sleep(transition::backoff(attempt));
        tokio::pin!(sleep);
        if reset && self.watch_lost {
            match self.cursor.as_mut().map(Cursor::recover) {
                Some(Ok(events)) => {
                    self.watch_lost = false;
                    (self.registration).report(&HoldCause::BindingUnreadable, false);
                    if !events.is_empty() {
                        return;
                    }
                }
                _ => return sleep.await,
            }
        }
        while let Some(cursor) = self.cursor.as_mut().filter(|_| reset) {
            let woke = tokio::select! {
                () = &mut sleep => return,
                woke = cursor.wake() => woke,
            };
            match woke {
                Ok(events) if events.is_empty() => {}
                Ok(_) => return,
                Err(_) => {
                    self.watch_lost = true;
                    (self.registration).report(&HoldCause::BindingUnreadable, true);
                    break;
                }
            }
        }
        sleep.await;
    }

    /// S6 and S7: a ledger request moves the population; a Legacy request only finishes a
    /// committed move before handing rows back.
    async fn transition(
        &mut self,
        closing: &Arc<Closing>,
        history: bool,
    ) -> Result<Landing, HoldCause> {
        let legacy = self.config.request == Request::Legacy;
        for attempt in 0..BUDGET {
            if attempt > 0 {
                tokio::time::sleep(transition::backoff(attempt - 1)).await;
            }
            let host = match self.host.take() {
                Some(host) => host,
                None => (self.ports.transition(closing.clone()))
                    .map_err(|_| held("move_unavailable"))?,
            };
            let movement = self.movement.take();
            let (mut host, movement, outcome) = loan(&mut self.slot, move |lease| {
                let mut host = host;
                let prepared = movement.map_or_else(|| Move::prepare(lease, &mut host), Ok);
                let (movement, outcome) = match prepared {
                    Err(_) => (None, Outcome::Held),
                    Ok(movement) if legacy && movement.is_fresh() => (None, Outcome::Legacy),
                    Ok(mut movement) => {
                        let outcome = movement.advance(lease, &mut host);
                        (Some(movement), outcome)
                    }
                };
                (host, movement, outcome)
            })
            .await
            .ok_or(held("supervisor_lost"))?;
            for (key, reason) in P::take_notices(&mut host) {
                self.notify(key, reason).await;
            }
            self.host = Some(host);
            self.movement = movement;
            match (outcome, legacy) {
                (Outcome::Held, _) => self.registration.report(&held("move_held"), true),
                (_, true) => return self.handback(closing).await,
                (Outcome::Ledger, false) => {
                    self.registration.report(&held("move_held"), false);
                    self.admitted = true;
                    return Ok(Landing::Admitted);
                }
                (Outcome::Legacy, false) => {
                    (self.registration.reopened(closing, history))
                        .map_err(|_| held("move_refused"))?;
                    return Ok(Landing::Legacy);
                }
            }
        }
        Err(held("move_held"))
    }

    /// Entered only after the move finished; releasing protection waits for queue retirement.
    async fn handback(&mut self, closing: &Arc<Closing>) -> Result<Landing, HoldCause> {
        #[cfg(test)]
        {
            self.handbacks += 1;
        }
        closing
            .begin_handback()
            .map_err(|_| held("handback_mode"))?;
        let host = self.host.take().ok_or(held("move_unavailable"))?;
        let (mut host, outcome) = loan(&mut self.slot, move |lease| {
            let mut host = host;
            let outcome = transition::handback(lease, &mut host);
            (host, outcome)
        })
        .await
        .ok_or(held("supervisor_lost"))?;
        for (key, reason) in P::take_notices(&mut host) {
            self.notify(key, reason).await;
        }
        self.host = Some(host);
        match outcome {
            Ok(Outcome::Legacy) => Ok(Landing::HandedBack),
            _ => Err(held("handback_held")),
        }
    }

    /// One Notice per cause and key in a supervisor's life; a failed send stays unsent.
    async fn notify(&mut self, key: Option<u64>, reason: &'static str) {
        let episode = (key, reconcile::topic(reason));
        if !self.sent.contains(&episode) && self.ports.notice(reconcile::notice(key, reason)).await
        {
            self.sent.insert(episode);
        }
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
#[path = "supervisor_tests.rs"]
mod tests;
