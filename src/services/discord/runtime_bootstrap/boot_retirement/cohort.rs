use super::completion::receive;
use super::publication::{Observations, SealReceipt};
use super::{
    BootPhase, BootPublication, BootRetirementHealth, BootRoster, BootSlotState, BootWorkFailure,
    BootWorkOnce, Completed,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
use tokio::{
    sync::watch,
    time::{Duration, Instant},
};

static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

struct State {
    slots: BTreeMap<String, BootSlotState>,
    claimed: BTreeSet<String>,
    started: Option<Instant>,
    phase: BootPhase,
    confirming: bool,
    timed_out: bool,
    failure: Option<BootWorkFailure>,
}

pub struct BootCohort {
    pub(super) epoch: u64,
    roster: BootRoster,
    state: Mutex<State>,
    changes: watch::Sender<()>,
    observations: Arc<Mutex<Observations>>,
    confirmation: BootWorkOnce<Result<SealReceipt, BootWorkFailure>>,
}

pub struct BootSlot {
    pub(super) cohort: Arc<BootCohort>,
    id: String,
}

impl BootCohort {
    pub(super) fn new(roster: BootRoster) -> Self {
        let empty = roster
            .providers
            .values()
            .all(|selection| selection.turn_channels.is_empty());
        let slots = roster
            .bots
            .keys()
            .map(|id| (id.clone(), BootSlotState::Preparing))
            .collect();
        Self {
            epoch: NEXT_EPOCH.fetch_add(1, Ordering::Relaxed),
            roster,
            state: Mutex::new(State {
                slots,
                claimed: BTreeSet::new(),
                started: None,
                phase: if empty {
                    BootPhase::Released
                } else {
                    BootPhase::Collecting
                },
                confirming: empty,
                timed_out: false,
                failure: None,
            }),
            changes: watch::channel(()).0,
            observations: Arc::new(Mutex::new(Observations::default())),
            confirmation: BootWorkOnce::default(),
        }
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn try_start_confirmation(
        self: &Arc<Self>,
        mut callback: impl FnMut(&str, &mut BootPublication) -> Result<(), BootWorkFailure>
        + Send
        + 'static,
    ) -> bool {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.confirming
                || state.failure.is_some()
                || !state
                    .slots
                    .values()
                    .all(|s| matches!(s, BootSlotState::Reaped | BootSlotState::ExcludedNoRuntime))
            {
                return false;
            }
            state.confirming = true;
            state.phase = BootPhase::Confirming;
        }
        self.changes.send_replace(());
        let cohort = self.clone();
        let worker = self.clone();
        let receiver = self
            .confirmation
            .start(self.epoch, "confirmation", move || {
                let mut publication = BootPublication::new(
                    worker.epoch,
                    worker.roster.providers.clone(),
                    worker.observations.clone(),
                );
                for provider in worker.roster.providers.keys() {
                    publication.confirm(provider, &mut callback)?;
                }
                publication.seal()
            });
        tokio::spawn(async move {
            let result = receive(receiver).await;
            let mut state = cohort.state.lock().unwrap_or_else(|e| e.into_inner());
            match result {
                Ok(completed)
                    if completed
                        .value()
                        .as_ref()
                        .is_ok_and(|receipt| receipt.matches(cohort.epoch))
                        && state.failure.is_none() =>
                {
                    state.phase = BootPhase::Released
                }
                _ => {
                    state.phase = BootPhase::Held;
                    state
                        .failure
                        .get_or_insert(BootWorkFailure::Invalid("confirmation failed"));
                }
            }
            cohort.changes.send_replace(());
        });
        true
    }

    fn mark_timed_out(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.phase != BootPhase::Released {
            state.timed_out = true;
            state.phase = BootPhase::Held;
        }
        self.changes.send_replace(());
    }

    pub async fn wait_released(&self) -> Result<(), BootWorkFailure> {
        let mut receiver = self.changes.subscribe();
        loop {
            {
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(error) = &state.failure {
                    return Err(error.clone());
                }
                if state.phase == BootPhase::Released {
                    return Ok(());
                }
            }
            receiver
                .changed()
                .await
                .map_err(|_| BootWorkFailure::Closed)?;
        }
    }

    pub fn snapshot(&self) -> BootRetirementHealth {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let observations = self
            .observations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let waiting_bots: Vec<_> = state
            .slots
            .iter()
            .filter(|(_, s)| matches!(s, BootSlotState::Preparing | BootSlotState::Failed))
            .map(|(id, _)| id.clone())
            .collect();
        let waiting_providers = waiting_bots
            .iter()
            .map(|id| self.roster.bots[id].provider.clone())
            .chain(
                self.roster
                    .providers
                    .keys()
                    .filter(|p| !observations.completed.contains(p))
                    .cloned(),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        BootRetirementHealth {
            phase: state.phase,
            elapsed_ms: state
                .started
                .map_or(0, |start| start.elapsed().as_millis() as u64),
            timed_out: state.timed_out,
            expected: state.slots.len(),
            reaped: state
                .slots
                .values()
                .filter(|s| **s == BootSlotState::Reaped)
                .count(),
            excluded: state
                .slots
                .values()
                .filter(|s| **s == BootSlotState::ExcludedNoRuntime)
                .count(),
            failed: state
                .slots
                .values()
                .filter(|s| **s == BootSlotState::Failed)
                .count(),
            waiting_bots,
            waiting_providers,
            completed_providers: observations.completed,
            published_keys: observations.published,
            refused_keys: observations.refused,
            failure: state.failure.as_ref().map(|e| format!("{e:?}")),
            supervisors_released: state.phase == BootPhase::Released,
        }
    }
}

impl BootSlot {
    pub fn begin(cohort: &Arc<BootCohort>, id: &str) -> Result<Self, BootWorkFailure> {
        let mut state = cohort.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.slots.contains_key(id) || !state.claimed.insert(id.to_owned()) {
            return Err(BootWorkFailure::Invalid("unknown or claimed slot"));
        }
        if state.started.is_none() {
            let start = Instant::now();
            state.started = Some(start);
            let cohort = cohort.clone();
            tokio::spawn(async move {
                tokio::time::sleep_until(start + Duration::from_secs(120)).await;
                cohort.mark_timed_out();
            });
        }
        Ok(Self {
            cohort: cohort.clone(),
            id: id.to_owned(),
        })
    }

    pub(super) fn provider(&self) -> &str {
        &self.cohort.roster.bots[&self.id].provider
    }

    pub fn arrive_reaped<T>(&self, completed: &Completed<T>) -> Result<(), BootWorkFailure> {
        if !completed.matches(self.cohort.epoch, self.provider()) {
            return Err(BootWorkFailure::Invalid("foreign completion receipt"));
        }
        self.transition(BootSlotState::Reaped)
    }

    pub fn exclude_no_runtime(&self) -> Result<(), BootWorkFailure> {
        if !self.cohort.roster.bots[&self.id].utility {
            return Err(BootWorkFailure::Invalid("runtime slot cannot be excluded"));
        }
        self.transition(BootSlotState::ExcludedNoRuntime)
    }

    fn transition(&self, next: BootSlotState) -> Result<(), BootWorkFailure> {
        let mut state = self.cohort.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.slots[&self.id] != BootSlotState::Preparing {
            return Err(BootWorkFailure::Invalid("slot already arrived"));
        }
        state.slots.insert(self.id.clone(), next);
        self.cohort.changes.send_replace(());
        Ok(())
    }

    pub fn fail(&self, error: BootWorkFailure) {
        let mut state = self.cohort.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.phase != BootPhase::Released {
            state.slots.insert(self.id.clone(), BootSlotState::Failed);
            state.failure.get_or_insert(error);
            state.phase = BootPhase::Held;
            self.cohort.changes.send_replace(());
        }
    }
}

impl Drop for BootSlot {
    fn drop(&mut self) {
        self.fail(BootWorkFailure::Invalid(
            "slot guard dropped before release",
        ));
    }
}
