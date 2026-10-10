use super::BootPhase::{Confirming, Held, Released};
use super::BootSlotState::{ExcludedNoRuntime, Failed, Preparing, Reaped};
use super::BootWorkFailure::Invalid;
use super::{
    BootPublication, BootResult, BootRetirementHealth, BootRoster, BootSlotState, BootWorkFailure,
    BootWorkOnce, Completed,
};
use std::{collections::BTreeMap, sync::Arc};
use tokio::{
    sync::watch,
    time::{Duration, Instant},
};

#[derive(Default)]
pub(super) struct State {
    slots: Vec<(bool, BootSlotState)>,
    started: Option<Instant>,
    confirming: bool,
    failure: Option<BootWorkFailure>,
    pub(super) health: BootRetirementHealth,
}
pub struct BootCohort<T> {
    pub(super) epoch: u64,
    pub(super) roster: BootRoster,
    workers: BTreeMap<String, BootWorkOnce<T>>,
    pub(super) state: watch::Sender<State>,
}
pub struct BootSlot<T> {
    cohort: Arc<BootCohort<T>>,
    index: usize,
}
impl<T: Send + Sync + 'static> BootCohort<T> {
    pub(super) fn new(epoch: u64, roster: BootRoster) -> Self {
        let providers = &roster.providers;
        let empty = providers.values().all(|s| s.turn_channels.is_empty());
        let workers = providers
            .keys()
            .map(|p| (p.clone(), BootWorkOnce::new(epoch, p)))
            .collect();
        let mut state = State {
            slots: vec![(false, Preparing); roster.bots.len()],
            confirming: empty,
            ..Default::default()
        };
        if empty {
            state.health.phase = Released;
        }
        Self {
            epoch,
            roster,
            workers,
            state: watch::channel(state).0,
        }
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    fn update<R>(&self, update: impl FnOnce(&mut State) -> R) -> R {
        let mut result = None;
        self.state.send_modify(|state| result = Some(update(state)));
        result.unwrap()
    }
    pub fn try_start_confirmation(
        self: &Arc<Self>,
        mut callback: impl FnMut(&str, &mut BootPublication<'_>) -> BootResult<()> + Send + 'static,
    ) -> bool {
        if !self.update(|state| {
            let ready = state
                .slots
                .iter()
                .all(|(_, s)| matches!(s, Reaped | ExcludedNoRuntime));
            if state.confirming || state.failure.is_some() || !ready {
                return false;
            }
            state.confirming = true;
            state.health.phase = Confirming;
            true
        }) {
            return false;
        }
        let cohort = self.clone();
        tokio::spawn(async move {
            let worker = cohort.clone();
            let result = tokio::task::spawn_blocking(move || {
                let mut publication = BootPublication::new(&worker);
                for provider in worker.roster.providers.keys() {
                    publication.confirm(provider, &mut callback)?;
                }
                publication.seal()
            })
            .await;
            let result = result
                .map_err(|error| BootWorkFailure::Worker(error.to_string()))
                .and_then(|report| report)
                .map(|seal| seal.matches(cohort.epoch));
            cohort.update(|s| {
                s.health.phase = if result == Ok(true) && s.failure.is_none() {
                    Released
                } else {
                    s.failure
                        .get_or_insert(result.err().unwrap_or(Invalid("invalid seal")));
                    Held
                };
            });
        });
        true
    }
    fn mark_timed_out(&self) {
        self.update(|state| {
            if state.health.phase != Released {
                state.health.timed_out = true;
                state.health.phase = Held;
            }
        });
    }
    pub async fn wait_released(&self) -> BootResult<()> {
        let mut rx = self.state.subscribe();
        loop {
            {
                let state = rx.borrow_and_update();
                if let Some(error) = &state.failure {
                    return Err(error.clone());
                }
                if state.health.phase == Released {
                    return Ok(());
                }
            }
            rx.changed().await.map_err(|_| BootWorkFailure::Closed)?;
        }
    }
    pub fn snapshot(&self) -> BootRetirementHealth {
        let s = self.state.borrow();
        let mut h = s.health.clone();
        h.expected = s.slots.len();
        h.elapsed_ms = s
            .started
            .map_or(0, |start| start.elapsed().as_millis() as u64);
        for ((_, status), bot) in s.slots.iter().zip(&self.roster.bots) {
            match status {
                Reaped => h.reaped += 1,
                ExcludedNoRuntime => h.excluded += 1,
                Failed => h.failed += 1,
                Preparing => {}
            }
            if matches!(status, Preparing | Failed) {
                h.waiting_bots.push(bot.slot.clone());
            }
        }
        let providers = &self.roster.providers;
        h.waiting_providers = providers
            .keys()
            .filter(|p| !h.completed_providers.contains(p))
            .cloned()
            .collect();
        h.failure = s.failure.as_ref().map(|e| format!("{e:?}"));
        h.supervisors_released = h.phase == Released;
        h
    }
}
impl<T: Send + Sync + 'static> BootSlot<T> {
    pub fn begin(cohort: &Arc<BootCohort<T>>, id: &str) -> BootResult<Self> {
        let bots = &cohort.roster.bots;
        let index = bots
            .iter()
            .position(|bot| bot.slot == id)
            .ok_or(Invalid("unknown slot"))?;
        let start = cohort.update(|state| {
            if state.slots[index].0 {
                return Err(Invalid("claimed slot"));
            }
            state.slots[index].0 = true;
            let start = state.started.is_none().then(Instant::now);
            if let Some(start) = start {
                state.started = Some(start);
            }
            Ok(start)
        })?;
        if let Some(start) = start {
            let cohort = cohort.clone();
            tokio::spawn(async move {
                tokio::time::sleep_until(start + Duration::from_secs(120)).await;
                cohort.mark_timed_out();
            });
        }
        Ok(Self {
            cohort: cohort.clone(),
            index,
        })
    }
    pub fn work_once(&self) -> Option<&BootWorkOnce<T>> {
        let bot = &self.cohort.roster.bots[self.index];
        self.cohort.workers.get(&bot.provider)
    }
    pub fn arrive_reaped(&self, completed: &Completed<T>) -> BootResult<()> {
        let bot = &self.cohort.roster.bots[self.index];
        if !completed.matches(self.cohort.epoch, &bot.provider) {
            return Err(Invalid("foreign completion receipt"));
        }
        self.transition(Reaped)
    }
    pub fn exclude_no_runtime(&self) -> BootResult<()> {
        if !self.cohort.roster.bots[self.index].utility {
            return Err(Invalid("runtime slot cannot be excluded"));
        }
        self.transition(ExcludedNoRuntime)
    }
    fn transition(&self, next: BootSlotState) -> BootResult<()> {
        self.cohort.update(|state| {
            if state.slots[self.index].1 != Preparing {
                return Err(Invalid("slot already arrived"));
            }
            state.slots[self.index].1 = next;
            Ok(())
        })
    }
}
impl<T> BootSlot<T> {
    pub fn fail(&self, error: BootWorkFailure) {
        self.cohort.state.send_modify(|s| {
            if s.health.phase != Released {
                s.slots[self.index].1 = Failed;
                s.failure.get_or_insert(error);
                s.health.phase = Held;
            }
        });
    }
}
impl<T> Drop for BootSlot<T> {
    fn drop(&mut self) {
        self.fail(Invalid("slot dropped"));
    }
}
