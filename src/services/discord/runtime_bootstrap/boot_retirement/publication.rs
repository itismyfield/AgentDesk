use super::{BootSelection, BootWorkFailure};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub(super) struct Observations {
    pub completed: Vec<String>,
    pub published: Vec<(String, u64)>,
    pub refused: Vec<((String, u64), String)>,
}

pub(super) struct SealReceipt {
    epoch: u64,
}

impl SealReceipt {
    pub(super) fn matches(&self, epoch: u64) -> bool {
        self.epoch == epoch
    }
}

pub struct BootPublication {
    epoch: u64,
    providers: BTreeMap<String, BootSelection>,
    current: Option<String>,
    sealed: bool,
    observations: Arc<Mutex<Observations>>,
}

impl BootPublication {
    pub(super) fn new(
        epoch: u64,
        providers: BTreeMap<String, BootSelection>,
        observations: Arc<Mutex<Observations>>,
    ) -> Self {
        Self {
            epoch,
            providers,
            current: None,
            sealed: false,
            observations,
        }
    }

    pub(super) fn confirm(
        &mut self,
        provider: &str,
        callback: &mut impl FnMut(&str, &mut Self) -> Result<(), BootWorkFailure>,
    ) -> Result<(), BootWorkFailure> {
        if self.sealed
            || !self.providers.contains_key(provider)
            || self
                .observations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .completed
                .iter()
                .any(|p| p == provider)
        {
            return Err(BootWorkFailure::Invalid("invalid provider confirmation"));
        }
        self.current = Some(provider.to_owned());
        let result = callback(provider, self);
        self.current = None;
        result?;
        self.observations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .completed
            .push(provider.to_owned());
        Ok(())
    }

    pub fn publish_with(
        &mut self,
        epoch: u64,
        provider: &str,
        channel: u64,
        commit: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), BootWorkFailure> {
        let provider = provider.to_ascii_lowercase();
        if self.sealed
            || epoch != self.epoch
            || self.current.as_deref() != Some(&provider)
            || !self.providers[&provider].turn_channels.contains(&channel)
        {
            return Err(BootWorkFailure::Invalid("publication outside boot scope"));
        }
        match commit() {
            Ok(()) => self
                .observations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .published
                .push((provider, channel)),
            Err(reason) => self.record_refusal(&provider, channel, reason),
        }
        Ok(())
    }

    fn record_refusal(&mut self, provider: &str, channel: u64, reason: String) {
        self.observations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .refused
            .push(((provider.to_ascii_lowercase(), channel), reason));
    }

    pub(super) fn seal(&mut self) -> Result<SealReceipt, BootWorkFailure> {
        if self.sealed
            || self
                .observations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .completed
                .len()
                != self.providers.len()
        {
            return Err(BootWorkFailure::Invalid(
                "providers incomplete or already sealed",
            ));
        }
        self.sealed = true;
        Ok(SealReceipt { epoch: self.epoch })
    }
}
