use super::BootWorkFailure::Invalid;
use super::cohort::State;
use super::{BootCohort, BootResult, BootSelection};
use std::collections::BTreeMap;
use tokio::sync::watch;

pub(super) struct SealReceipt {
    epoch: u64,
}
impl SealReceipt {
    pub(super) fn matches(&self, epoch: u64) -> bool {
        self.epoch == epoch
    }
}
pub struct BootPublication<'a> {
    epoch: u64,
    providers: &'a BTreeMap<String, BootSelection>,
    state: &'a watch::Sender<State>,
    current: Option<&'a str>,
    sealed: bool,
}
impl<'a> BootPublication<'a> {
    pub(super) fn new<T>(cohort: &'a BootCohort<T>) -> Self {
        Self {
            epoch: cohort.epoch,
            providers: &cohort.roster.providers,
            state: &cohort.state,
            current: None,
            sealed: false,
        }
    }
    pub(super) fn confirm(
        &mut self,
        provider: &'a str,
        callback: &mut impl FnMut(&str, &mut Self) -> BootResult<()>,
    ) -> BootResult<()> {
        if self.sealed || !self.providers.contains_key(provider) {
            return Err(Invalid("invalid provider confirmation"));
        }
        self.current = Some(provider);
        let result = callback(provider, self);
        self.current = None;
        result?;
        self.state
            .send_modify(|state| state.health.completed_providers.push(provider.into()));
        Ok(())
    }
    pub fn publish_with(
        &mut self,
        epoch: u64,
        provider: &str,
        channel: u64,
        commit: impl FnOnce() -> Result<(), String>,
    ) -> BootResult<()> {
        let provider = provider.to_ascii_lowercase();
        let selected = self.providers.get(&provider);
        if self.sealed
            || epoch != self.epoch
            || self.current != Some(provider.as_str())
            || !selected.is_some_and(|s| s.turn_channels.contains(&channel))
        {
            return Err(Invalid("publication outside boot scope"));
        }
        let result = commit();
        self.state.send_modify(|s| match result {
            Ok(()) => s.health.published_keys.push((provider, channel)),
            Err(reason) => s.health.refused_keys.push(((provider, channel), reason)),
        });
        Ok(())
    }
    pub(super) fn seal(&mut self) -> BootResult<SealReceipt> {
        if self.sealed
            || self.state.borrow().health.completed_providers.len() != self.providers.len()
        {
            return Err(Invalid("providers incomplete or already sealed"));
        }
        self.sealed = true;
        Ok(SealReceipt { epoch: self.epoch })
    }
}
#[cfg(test)]
#[path = "publication_tests.rs"]
mod tests;
