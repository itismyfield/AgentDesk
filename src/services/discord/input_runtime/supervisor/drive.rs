//! S8 onward: input's reading of one channel's binding log, the life of the facts and actor built
//! on it, the readiness gate, and the actor's one step per pass.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{OwnedMutexGuard, mpsc};

use crate::services::discord::input_runtime::supervisor::admission::Admission;
pub(crate) use crate::services::discord::input_runtime::supervisor::command::SupervisorCmd;

use super::{BUDGET, Cursor, Landing, Ports, Supervisor, clear_loan, held, loan};
use crate::services::discord::input_runtime::clear::{self, ClearHost, Outcome, Unresolved};
use crate::services::discord::input_runtime::fence::{self, Closing};
use crate::services::discord::input_runtime::reconcile::{self, HoldCause};
use crate::services::discord::reaction_lifecycle::is_real_discord_message_id_value;
use crate::services::tui_input::actor::pane::Pane;
use crate::services::tui_input::actor::{self, InputActor, READY_WINDOW, Step};
use crate::services::tui_input::ledger::LedgerSlot;
use crate::services::tui_input::rows::RowState;
use crate::services::tui_input::transition;
use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding, SourceId};
use crate::services::tui_o::writer::adoption;
use crate::services::tui_o::writer::binding::{BindingEvent, BindingRecord, BindingTarget};
use crate::services::tui_o::writer::input_facts::reactions::{self, InputReaction, ReactionPort};
use crate::services::tui_o::writer::input_facts::{ChannelFact, InputFacts, TurnState};

/// How the log reached its current binding; input builds only on `Eligible`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Eligibility {
    #[default]
    Absent,
    Pending,
    Rejected,
    EmptyNonce,
    Eligible,
}

/// The binding facts and an actor are built for: O's current source, its pane and the nonce.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ViewKey {
    pub(crate) provider: ShadowProvider,
    pub(crate) source: SourceId,
    pub(crate) tmux_session: String,
    pub(crate) execution_nonce: String,
}

/// O's binding gated by input: the Pendings O still waits on and how the last record moved it.
#[derive(Clone, Debug, Default)]
pub(crate) struct InputBindingView {
    applied: Vec<BindingEvent>,
    waiting: Vec<BindingEvent>,
    eligibility: Eligibility,
    nonce: String,
}

impl InputBindingView {
    pub(crate) fn of(events: &[BindingEvent]) -> Self {
        let mut view = Self::default();
        for event in events {
            view.apply(event.clone());
        }
        view
    }

    /// Folds one record; the waiting list drops exactly what O drops, by O's own `supersedes`.
    pub(crate) fn apply(&mut self, event: BindingEvent) {
        let eligibility = match &event.record {
            BindingRecord::Bound { new, .. } => {
                self.waiting
                    .retain(|waiting| !adoption::supersedes(waiting, &event));
                match new {
                    BindingTarget::Pending { .. } => {
                        self.waiting.push(event.clone());
                        Eligibility::Pending
                    }
                    _ if !self.waiting.is_empty() => Eligibility::Pending,
                    _ if event.execution_nonce.is_empty() => Eligibility::EmptyNonce,
                    _ => Eligibility::Eligible,
                }
            }
            BindingRecord::Resolved {
                resolves_seq,
                source,
            } => {
                let at = self.waiting.iter().position(|w| w.seq == *resolves_seq);
                let named = at.map(|at| self.waiting.remove(at));
                if !self.waiting.is_empty() {
                    Eligibility::Pending
                } else if named.is_some_and(|pending| resolves(&pending, &event, source)) {
                    Eligibility::Eligible
                } else {
                    Eligibility::Rejected
                }
            }
            BindingRecord::Rejected { .. } => Eligibility::Rejected,
        };
        if eligibility == Eligibility::Eligible {
            self.nonce.clone_from(&event.execution_nonce);
        }
        self.eligibility = eligibility;
        self.applied.push(event);
    }

    /// O's current source and pane over every record applied so far.
    pub(crate) fn identity(&self) -> Option<(SourceId, String)> {
        adoption::current(&self.applied)
    }

    pub(crate) fn eligibility(&self) -> Eligibility {
        self.eligibility
    }

    pub(crate) fn key(&self) -> Option<ViewKey> {
        if self.eligibility != Eligibility::Eligible {
            return None;
        }
        let (source, tmux_session) = self.identity()?;
        Some(ViewKey {
            provider: self.applied.last()?.provider,
            source,
            tmux_session,
            execution_nonce: self.nonce.clone(),
        })
    }
}

/// A Resolved names its Pending only if pane, nonce and every anchor the Pending carried agree.
fn resolves(pending: &BindingEvent, resolved: &BindingEvent, source: &SourceId) -> bool {
    let BindingRecord::Bound {
        new:
            BindingTarget::Pending {
                payload_session_id: session,
                payload_transcript_path: path,
            },
        ..
    } = &pending.record
    else {
        return false;
    };
    let no_path = path.as_os_str().is_empty();
    resolved.tmux_session == pending.tmux_session
        && !resolved.execution_nonce.is_empty()
        && resolved.execution_nonce == pending.execution_nonce
        && !(session.is_empty() && no_path)
        && (session.is_empty() || source.session_id == *session)
        && (no_path || source.path == *path)
}

/// What the drive built for one key, with only the facts this instance polled itself.
pub(crate) struct Instance<T> {
    pub(crate) key: ViewKey,
    pub(crate) built: T,
    fact: Option<ChannelFact>,
}

impl<T> Instance<T> {
    pub(crate) fn polled(&mut self, fact: ChannelFact) {
        self.fact = Some(fact);
    }

    pub(crate) fn fact(&self) -> Option<&ChannelFact> {
        self.fact.as_ref()
    }
}

/// The view and the one instance it allows; any record may drop it or build a fresh one.
pub(crate) struct Lifecycle<T> {
    view: InputBindingView,
    instance: Option<Instance<T>>,
}

impl<T> Default for Lifecycle<T> {
    fn default() -> Self {
        Self {
            view: InputBindingView::default(),
            instance: None,
        }
    }
}

impl<T> Lifecycle<T> {
    /// Applies a wake's records in seq order and settles the instance after each one, since a
    /// record inside the batch can end a binding the batch's last record restores.
    pub(crate) fn apply(
        &mut self,
        events: Vec<BindingEvent>,
        mut build: impl FnMut(&ViewKey, u64) -> T,
    ) {
        for event in events {
            let seq = event.seq;
            self.view.apply(event);
            let key = self.view.key();
            if (self.instance.as_ref()).is_some_and(|held| key.as_ref() != Some(&held.key)) {
                self.instance = None;
            }
            if let Some(key) = key.filter(|_| self.instance.is_none()) {
                let built = build(&key, seq);
                self.instance = Some(Instance {
                    key,
                    built,
                    fact: None,
                });
            }
        }
    }

    pub(crate) fn view(&self) -> &InputBindingView {
        &self.view
    }

    pub(crate) fn instance(&mut self) -> Option<&mut Instance<T>> {
        self.instance.as_mut()
    }

    /// Takes the instance out for one blocking step; no record applies until it is restored.
    fn lend(&mut self) -> Option<Instance<T>> {
        self.instance.take()
    }

    fn restore(&mut self, instance: Instance<T>) {
        self.instance = Some(instance);
    }
}

/// A hold must last this long before health and a Notice report it.
pub(crate) const HOLD_GRACE: Duration = READY_WINDOW;
/// The pass interval when no binding wake or command arrives.
pub(crate) const TICK: Duration = Duration::from_secs(1);
const POLL_BYTES: u64 = 1 << 20;

/// What the drive needs beyond the supervisor's ports; production builds them in G2.
pub(crate) trait DrivePorts: Send + 'static {
    type Pane: Pane + Send + 'static;
    type Reactions: ReactionPort;
    /// The pane of `key`'s tmux session; `None` while it cannot be reached.
    fn pane(&mut self, key: &ViewKey) -> Option<Self::Pane>;
    fn reactions(&self) -> &Self::Reactions;
}

/// Facts and actor of one eligible binding; each is built on demand and retried while missing.
pub(crate) struct Built<Pn> {
    facts: Option<InputFacts>,
    actor: Option<InputActor<Pn>>,
}

#[derive(Default)]
pub(super) struct Clearing {
    retries: u32,
    at: Option<Instant>,
    reset: bool,
}

#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct Counts {
    pub(crate) opens: usize,
    pub(crate) holds: usize,
    pub(crate) builds: usize,
    pub(crate) held_steps: usize,
    pub(crate) creations: Vec<u64>,
}

pub(crate) struct ChannelDrive<D: DrivePorts> {
    ports: D,
    life: Lifecycle<Built<D::Pane>>,
    closing: Arc<Closing>,
    ready: bool,
    unreadable: bool,
    lost: bool,
    pub(super) clearing: Option<Clearing>,
    hold: Option<(&'static str, Instant)>,
    #[cfg(test)]
    pub(crate) counts: Counts,
}

impl<D: DrivePorts> ChannelDrive<D> {
    pub(crate) fn view(&self) -> &InputBindingView {
        self.life.view()
    }
}

/// A drive dropped without [`Supervisor::stop_drive`], by cancellation or a panic, still holds an
/// open gate. A loan it abandoned keeps the slot lent, so that worker's writes stay its own.
impl<D: DrivePorts> Drop for ChannelDrive<D> {
    fn drop(&mut self) {
        if self.ready {
            let _ = self.closing.hold();
        }
    }
}

impl Cursor {
    /// Rereads the whole log; events read during boot were consumed there, not applied.
    fn replay(&mut self) -> Result<Vec<BindingEvent>, String> {
        self.seq = 0;
        self.read()
    }

    /// A pass without a wake applies nothing; after a failed read it resubscribes and rereads,
    /// so the failure clears only on a read that works.
    fn reread(&mut self, unreadable: bool) -> Result<Vec<BindingEvent>, String> {
        match unreadable {
            true => self.recover(),
            false => Ok(Vec::new()),
        }
    }
}

fn binding_hold(eligibility: Eligibility) -> &'static str {
    match eligibility {
        Eligibility::Pending => "binding_pending",
        Eligibility::Rejected => "binding_rejected",
        Eligibility::EmptyNonce => "binding_empty_nonce",
        Eligibility::Absent | Eligibility::Eligible => "binding_absent",
    }
}

/// [`clear_loan`] for a user clear: the worker runs the clear itself, not a resume.
async fn start_clear<H: ClearHost>(
    slot: &mut LedgerSlot,
    host: H,
    guard: OwnedMutexGuard<()>,
) -> Option<Outcome> {
    let ledger = match slot.lend().ok()?.into_ledger() {
        Ok(ledger) => ledger,
        Err(_) => {
            slot.restore_fresh();
            return None;
        }
    };
    match clear::start(ledger, host, guard).await {
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

/// A started drive and the binding log replayed for its first pass.
type Started<D> = (ChannelDrive<D>, Result<Vec<BindingEvent>, String>);

impl<P: Ports> Supervisor<P> {
    /// Holds the reservation before adoption and keeps a held task alive after its ACK.
    pub(crate) async fn serve<D: DrivePorts>(
        &mut self,
        ports: D,
        expected: &[crate::services::provider::ProviderKind],
        mut views: tokio::sync::oneshot::Receiver<Vec<Arc<crate::services::discord::SharedData>>>,
        mut commands: mpsc::Receiver<SupervisorCmd>,
    ) -> bool {
        self.prepare_handoff();
        let mut final_views = None;
        let mut waiting_views = true;
        while !self.adopted || waiting_views {
            tokio::select! {
                supplied = &mut views, if waiting_views => {
                    final_views = Some(supplied.unwrap_or_default());
                    waiting_views = false;
                }
                command = commands.recv() => match command {
                    Some(command @ SupervisorCmd::Adopt { .. }) => self.input_command(command, false).await,
                    Some(SupervisorCmd::Wake { .. }) => {},
                    Some(_) => {},
                    None => return false,
                }
            }
        }
        let probe = self.ports.mapping();
        let runtimes = final_views.unwrap_or_default();
        let runtimes = if expected.contains(&self.config.provider) {
            runtimes
        } else {
            Vec::new()
        };
        probe.adopt(expected, runtimes);
        loop {
            let landing = self.boot().await;
            self.retry_notices().await;
            match landing {
                Landing::Admitted => {
                    self.run(ports, commands).await;
                    return false;
                }
                Landing::Legacy | Landing::HandedBack => return true,
                Landing::Held(_) => {}
            }
            tokio::select! {
                command = commands.recv() => match command {
                    Some(SupervisorCmd::Wake { cause: _ }) => {},
                    Some(command) => self.input_command(command, false).await,
                    None => return false,
                },
                () = tokio::time::sleep(TICK) => {},
            }
        }
    }

    /// S1 to S7; exhaustion and Close are process-local admission barriers.
    pub(crate) async fn boot(&mut self) -> Landing {
        self.admitted = false;
        let result = match self.boot_guard() {
            Err(cause) => Err(cause),
            Ok(()) => {
                self.invalidate_order();
                match self.boot_guard() {
                    Err(cause) => Err(cause),
                    Ok(()) => self.stages().await,
                }
            }
        };
        match result {
            Ok(landing) => landing,
            Err(cause) => {
                self.registration.report(&cause, true);
                Landing::Held(cause)
            }
        }
    }

    fn boot_guard(&self) -> Result<(), HoldCause> {
        if let Some(cause) = self.registration.latched() {
            return Err(cause);
        }
        let exhausted = self.order.exhausted();
        #[cfg(test)]
        let exhausted = exhausted && !super::mutant("boot_reopens_exhausted_admission");
        if exhausted {
            return Err(held("generation_exhausted"));
        }
        if self.closed {
            return Err(held("closed"));
        }
        Ok(())
    }

    pub(super) fn invalidate_order(&mut self) {
        match self
            .admission_gen
            .checked_add(1)
            .filter(|epoch| *epoch != u64::MAX)
        {
            Some(epoch) => {
                self.admission_gen = epoch;
                self.order.invalidate(epoch);
            }
            None => self.order.exhaust(),
        }
        if self.order.exhausted() {
            self.admitted = false;
        }
    }

    pub(super) async fn close_command(
        &mut self,
        ack: tokio::sync::oneshot::Sender<
            Result<super::command::ClosedAndFlushed, super::receipt::Deferred>,
        >,
    ) {
        use super::receipt::Deferred;
        self.closed = true;
        self.invalidate_order();
        let closing = {
            let closings = self
                .registration
                .registry
                .closings
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            closings.get(&self.registration.key).cloned()
        };
        #[cfg(test)]
        let observer = self.after_close_loan.get_mut().unwrap().take();
        let (result, flushed) = super::command::close_slot(
            &mut self.slot,
            closing,
            &self.config.provider,
            self.config.channel,
            ack,
            #[cfg(test)]
            observer,
        )
        .await;
        if result.is_ok() || matches!(result, Err(Deferred::Gate(_))) {
            self.registration
                .report(&held("ledger_close_gate_unconfirmed"), result.is_err());
        }
        if flushed
            || matches!(
                result,
                Err(Deferred::Persistence | Deferred::SupervisorLost)
            )
        {
            self.registration
                .report(&held("ledger_close_flush_unconfirmed"), !flushed);
        }
        #[cfg(test)]
        if result.is_err() && super::mutant("close_failure_reopens_admission") {
            self.closed = false;
            self.admitted = true;
        }
    }

    /// S8: a drive over the whole binding log so far. Nothing opens before its first pass, and
    /// nothing starts before admission.
    pub(crate) fn start_drive<D: DrivePorts>(&mut self, ports: D) -> Option<Started<D>> {
        if !self.admission_open() {
            return None;
        }
        let gate = fence::lookup(&self.config.provider, self.config.channel)?;
        let closing = self.registration.closing(&gate).ok()?;
        let replay = self.cursor.as_mut()?.replay();
        let drive = ChannelDrive {
            ports,
            life: Lifecycle::default(),
            closing,
            ready: false,
            unreadable: false,
            lost: false,
            clearing: None,
            hold: None,
            #[cfg(test)]
            counts: Counts::default(),
        };
        Some((drive, replay))
    }

    /// The loop from S8: each binding wake, command or tick runs one pass, until the command
    /// sender closes. Loans run in the pass, never inside the wait.
    pub(crate) async fn run<D: DrivePorts>(
        &mut self,
        ports: D,
        mut commands: mpsc::Receiver<SupervisorCmd>,
    ) {
        let Some((mut drive, mut woke)) = self.start_drive(ports) else {
            return;
        };
        loop {
            self.tick(&mut drive, woke, Instant::now()).await;
            let unreadable = drive.unreadable;
            let Some(cursor) = self.cursor.as_mut() else {
                return self.stop_drive(drive);
            };
            // An unreadable log waits for the tick's reread, but commands and their close still land.
            let command = tokio::select! {
                command = commands.recv() => command,
                events = cursor.wake(), if !unreadable => {
                    woke = events;
                    continue;
                }
                () = tokio::time::sleep(TICK) => {
                    woke = cursor.reread(unreadable);
                    continue;
                }
            };
            match command {
                Some(SupervisorCmd::Wake { cause: _ }) => {
                    woke = self
                        .cursor
                        .as_mut()
                        .map_or(Ok(Vec::new()), |cursor| cursor.recover());
                    continue;
                }
                Some(SupervisorCmd::Clear) => self.clear(&mut drive, Instant::now()).await,
                Some(SupervisorCmd::Close { ack }) => {
                    self.close_command(ack).await;
                    drive.ready = false;
                }
                Some(command) => {
                    let latest = self.cursor.as_mut().map_or(Ok(Vec::new()), |cursor| {
                        if drive.unreadable {
                            cursor.recover()
                        } else {
                            cursor.read()
                        }
                    });
                    if drive.unreadable || !latest.as_ref().is_ok_and(|events| events.is_empty()) {
                        self.tick(&mut drive, latest, Instant::now()).await;
                    }
                    let mode =
                        fence::lookup(&self.config.provider, self.config.channel).map(|g| g.mode());
                    let admission = self.receipt_admission(&drive, mode);
                    self.input_command(command, admission.receipt_open).await;
                }
                None => return self.stop_drive(drive),
            }
            woke = (self.cursor.as_mut()).map_or(Ok(Vec::new()), |c| c.reread(unreadable));
        }
    }

    pub(super) fn receipt_admission<D: DrivePorts>(
        &self,
        drive: &ChannelDrive<D>,
        mode: Option<fence::Mode>,
    ) -> Admission {
        let clear = drive.clearing.is_none();
        #[cfg(test)]
        let clear = clear || super::mutant("drop_clear_check");
        Admission::new(
            self.admission_open()
                && drive.ready
                && clear
                && !drive.lost
                && !drive.unreadable
                && matches!(
                    mode,
                    Some(fence::Mode::Frozen | fence::Mode::Held | fence::Mode::LedgerOpen)
                ),
            // Fresh pane and facts proof belongs to the actor's step.
            false,
        )
    }

    /// Ends the drive; a ready gate is held, so nothing submits until a later S8 opens it.
    pub(crate) fn stop_drive<D: DrivePorts>(&mut self, mut drive: ChannelDrive<D>) {
        self.invalidate_order();
        self.gate(&mut drive, false);
    }

    /// One pass: binding records one at a time, a due clear retry, readiness, then at most one
    /// actor step for the head row.
    pub(crate) async fn tick<D: DrivePorts>(
        &mut self,
        drive: &mut ChannelDrive<D>,
        woke: Result<Vec<BindingEvent>, String>,
        now: Instant,
    ) {
        self.retry_notices().await;
        if woke.is_err() || woke.as_ref().is_ok_and(|events| !events.is_empty()) {
            self.invalidate_order();
        }
        drive.unreadable = woke.is_err();
        let events = woke.unwrap_or_default();
        let advanced = !events.is_empty();
        #[cfg(test)]
        let creations = &mut drive.counts.creations;
        drive.life.apply(events, |_, _seq| {
            #[cfg(test)]
            creations.push(_seq);
            Built {
                facts: None,
                actor: None,
            }
        });
        let binding = HoldCause::BindingUnreadable;
        self.registration.report(&binding, drive.unreadable);
        if (drive.clearing.as_ref())
            .is_some_and(|c| c.at.is_some_and(|at| now >= at || (c.reset && advanced)))
        {
            if let Err(cause) = self.check_mapping(&drive.closing, "before_clear") {
                self.gate(drive, false);
                return self.registration.report(&cause, true);
            }
            let outcome = match self.ports.clear().await {
                Some((host, guard)) => clear_loan(&mut self.slot, host, guard).await,
                None => None,
            };
            if let Err(cause) = self.check_mapping(&drive.closing, "after_clear") {
                self.gate(drive, false);
                return self.registration.report(&cause, true);
            }
            self.settle_clear(drive, outcome, now).await;
        }
        let rows = loan(&mut self.slot, |lease| lease.get()?.rows()).await;
        let rows = rows.and_then(Result::ok);
        self.registration
            .report(&HoldCause::LedgerUnreadable, rows.is_none());
        let missing_identity = rows
            .as_ref()
            .is_some_and(|rows| rows.receipt_identity_missing());
        self.registration
            .report(&held("ledger_receipt_identity_missing"), missing_identity);
        let ready =
            self.admission_open() && drive.clearing.is_none() && !drive.lost && !missing_identity;
        self.gate(drive, ready && rows.is_some());
        let (Some(rows), true, false) = (rows, drive.ready, drive.unreadable) else {
            return;
        };
        if !self.order.offer_ready() {
            return;
        }
        let Some((key, row)) = actor::head(&rows) else {
            return self.hold(drive, None, now).await;
        };
        let open = rows.open_rows().count();
        if matches!(row.state, RowState::Held(_) | RowState::Unaccepted) {
            return self.row_held(drive, key, open).await;
        }
        let waiting = matches!(row.state, RowState::Received | RowState::Ready);
        let cause = |reason| Some((reason, key, open));
        let Some(mut instance) = drive.life.lend() else {
            let reason = binding_hold(drive.life.view().eligibility());
            return self.hold(drive, cause(reason), now).await;
        };
        let source = SourceBinding {
            channel_id: self.config.channel,
            provider: instance.key.provider,
            source: instance.key.source.clone(),
        };
        if instance.built.actor.is_none() {
            let pane = drive.ports.pane(&instance.key);
            instance.built.actor = pane.map(|pane| {
                #[cfg(test)]
                {
                    drive.counts.builds += 1;
                }
                InputActor::new(source.clone(), pane)
            });
        }
        if instance.built.actor.is_none() {
            drive.life.restore(instance);
            return self.hold(drive, cause("actor_unavailable"), now).await;
        }
        #[cfg(test)]
        if drive.life.view().key().is_none() {
            drive.counts.held_steps += 1;
        }
        let guard = self.guard();
        let stepped = loan(&mut self.slot, move |lease| {
            // A late edge stops the actor before it can submit; the row keeps its responsibility.
            if let Err(cause) = guard.check("offer") {
                return (instance, Err(cause));
            }
            let built = &mut instance.built;
            if built.facts.is_none() {
                built.facts = InputFacts::open(source).ok();
            }
            match built.facts.as_mut().map(|facts| facts.poll(POLL_BYTES)) {
                Some(Ok(fact)) => instance.fact = Some(fact),
                Some(Err(_)) => (built.facts, instance.fact) = (None, None),
                None => {}
            }
            let actor = built.actor.as_mut().expect("actor built before the step");
            let fact = instance.fact.as_ref();
            // The actor's tmux and file IO is blocking, so it runs only on this worker.
            let step = lease
                .get()
                .and_then(|ledger| futures::executor::block_on(actor.step(ledger, fact, now)));
            lease.needs_reopen |= step.is_err();
            (instance, Ok(step))
        })
        .await;
        let Some((instance, step)) = stepped else {
            drive.lost = true;
            self.gate(drive, false);
            return self.registration.report(&held("supervisor_lost"), true);
        };
        let step = match step {
            Ok(step) => step,
            Err(cause) => {
                drive.life.restore(instance);
                self.gate(drive, false);
                return self.registration.report(&cause, true);
            }
        };
        let unknown = !matches!(
            instance.fact().map(|fact| &fact.state),
            Some(TurnState::Open { .. } | TurnState::Idle)
        );
        drive.life.restore(instance);
        if let Ok(Step::Moved(key, RowState::Held(_) | RowState::Unaccepted)) = step {
            self.row_held(drive, key, open).await;
            return self.react(drive, key, InputReaction::Warning).await;
        }
        let reason = (waiting && unknown).then_some("facts_unknown");
        self.hold(drive, reason.and_then(cause), now).await;
        if let Ok(Step::Moved(key, RowState::Done(_))) = step {
            self.react(drive, key, InputReaction::Done).await;
        }
    }

    /// A user clear while driving: submission stops at once and resumes only once it settles.
    pub(crate) async fn clear<D: DrivePorts>(&mut self, drive: &mut ChannelDrive<D>, now: Instant) {
        if let Err(cause) = self.check_mapping(&drive.closing, "before_clear") {
            self.gate(drive, false);
            return self.registration.report(&cause, true);
        }
        self.invalidate_order();
        drive.clearing = Some(Clearing::default());
        self.gate(drive, false);
        let outcome = match self.ports.clear().await {
            Some((host, guard)) => start_clear(&mut self.slot, host, guard).await,
            None => None,
        };
        if let Err(cause) = self.check_mapping(&drive.closing, "after_clear") {
            self.gate(drive, false);
            return self.registration.report(&cause, true);
        }
        self.settle_clear(drive, outcome, now).await;
    }

    /// An unconfirmed reset retries on an advancing binding wake or after backoff, PG trouble
    /// after backoff, within the boot budget; any other hold waits for the next boot.
    async fn settle_clear<D: DrivePorts>(
        &mut self,
        drive: &mut ChannelDrive<D>,
        outcome: Option<Outcome>,
        now: Instant,
    ) {
        let Some(clearing) = drive.clearing.as_mut() else {
            return;
        };
        let retry = matches!(
            outcome,
            None | Some(Outcome::Held(
                Unresolved::ResetUnconfirmed
                    | Unresolved::PgUnavailable
                    | Unresolved::CommitUncertain
                    | Unresolved::ResolveUncertain
            ))
        );
        match outcome {
            Some(Outcome::Cleared | Outcome::Idle | Outcome::Refused(_)) => {
                drive.clearing = None;
                self.invalidate_order();
            }
            _ if retry && clearing.retries < BUDGET => {
                clearing.reset = outcome == Some(Outcome::Held(Unresolved::ResetUnconfirmed));
                clearing.at = Some(now + transition::backoff(clearing.retries));
                clearing.retries += 1;
            }
            _ if retry => {
                clearing.at = None;
                self.registration
                    .report(&held("clear_retry_exhausted"), true);
                self.notify(None, "clear_retry_exhausted").await;
            }
            _ => {
                clearing.at = None;
                self.registration.report(&held("clear_held"), true);
            }
        }
    }

    /// Opens the gate once when submission becomes ready and holds it once when it stops.
    pub(super) fn gate<D: DrivePorts>(&mut self, drive: &mut ChannelDrive<D>, ready: bool) {
        let frozen = fence::lookup(&self.config.provider, self.config.channel)
            .is_some_and(|gate| gate.mode() == fence::Mode::Frozen);
        if ready == drive.ready && (ready || !frozen) {
            return;
        }
        if ready && let Err(cause) = self.check_mapping(&drive.closing, "ledger_open") {
            drive.ready = false;
            return self.registration.report(&cause, true);
        }
        if ready {
            let advance = true;
            #[cfg(test)]
            let advance = advance && !super::mutant("drop_gate_generation");
            if advance {
                self.invalidate_order();
            }
            if !self.admission_open() {
                drive.ready = false;
                return;
            }
        }
        #[cfg(test)]
        match ready {
            true => drive.counts.opens += 1,
            false => drive.counts.holds += 1,
        }
        let moved = match ready {
            true => drive.closing.open_ledger(),
            false => drive.closing.hold(),
        };
        // The slot is shared with other transition holds, so only an opened gate clears it.
        if moved.is_err() || ready {
            self.registration.report(&held("gate_mode"), moved.is_err());
        }
        let advance = moved.is_ok();
        #[cfg(test)]
        let advance = advance && !super::mutant("drop_gate_generation");
        if advance && !ready {
            self.invalidate_order();
        }
        drive.ready = ready && moved.is_ok();
    }

    /// Reports a hold that outlasted the grace once per head row; a changed cause starts over.
    async fn hold<D: DrivePorts>(
        &mut self,
        drive: &mut ChannelDrive<D>,
        cause: Option<(&'static str, u64, usize)>,
        now: Instant,
    ) {
        let Some((reason, head, held)) = cause else {
            drive.hold = None;
            // Removal goes by slot, so any input cause clears the line.
            return (self.registration).report(&HoldCause::InputHeld("", 0, 0), false);
        };
        let since = match drive.hold {
            Some((current, since)) if current == reason => since,
            _ => {
                drive.hold = Some((reason, now));
                self.registration
                    .report(&HoldCause::InputHeld(reason, head, held), false);
                now
            }
        };
        if now.saturating_duration_since(since) < HOLD_GRACE {
            return;
        }
        let cause = HoldCause::InputHeld(reason, head, held);
        self.registration.report(&cause, true);
        let episode = (Some(head), "input_held");
        let text = reconcile::input_notice(head, held);
        if !self.sent.contains(&episode) && self.ports.notice(text).await {
            self.sent.insert(episode);
        }
    }

    /// A Held or Unaccepted head blocks every row behind it until someone settles it, so it stays
    /// in health and its Notice is retried each pass until one is delivered.
    async fn row_held<D: DrivePorts>(
        &mut self,
        drive: &mut ChannelDrive<D>,
        head: u64,
        held: usize,
    ) {
        drive.hold = None;
        let cause = HoldCause::InputHeld("row_held", head, held);
        self.registration.report(&cause, true);
        self.notify(Some(head), "row_held").await;
    }

    /// Synthetic inputs have no Discord message to react on; a failed reaction leaves the ledger
    /// as it is and the next state change repaints it.
    async fn react<D: DrivePorts>(&self, drive: &ChannelDrive<D>, key: u64, state: InputReaction) {
        if is_real_discord_message_id_value(key) {
            let port = drive.ports.reactions();
            let _ = reactions::reconcile(port, self.config.channel, key, state).await;
        }
    }
}

#[cfg(test)]
#[path = "drive_tests.rs"]
mod tests;
