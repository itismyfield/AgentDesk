//! Provider-keyed Discord channel bindings.
//!
//! Unknown keys intentionally round-trip so rolling deployments and future
//! provider adapters cannot silently discard configuration they do not yet
//! understand. Callers that require a supported provider must validate through
//! the canonical provider registry at their service boundary.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::AgentChannel;

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct AgentChannels {
    inner: BTreeMap<String, AgentChannel>,
}

impl AgentChannels {
    pub fn new() -> Self {
        Self::default()
    }

    fn normalize_key(raw: &str) -> String {
        raw.trim().to_ascii_lowercase()
    }

    pub fn get(&self, id: &str) -> Option<&AgentChannel> {
        self.inner.get(&Self::normalize_key(id))
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut AgentChannel> {
        self.inner.get_mut(&Self::normalize_key(id))
    }

    pub fn insert(&mut self, id: impl AsRef<str>, channel: AgentChannel) -> Option<AgentChannel> {
        self.inner.insert(Self::normalize_key(id.as_ref()), channel)
    }

    pub fn remove(&mut self, id: &str) -> Option<AgentChannel> {
        self.inner.remove(&Self::normalize_key(id))
    }

    pub fn contains_key(&self, id: &str) -> bool {
        self.inner.contains_key(&Self::normalize_key(id))
    }

    pub fn with(mut self, id: impl AsRef<str>, channel: AgentChannel) -> Self {
        self.insert(id, channel);
        self
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &AgentChannel)> {
        self.inner
            .iter()
            .map(|(key, channel)| (key.as_str(), channel))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.inner.keys().map(String::as_str)
    }

    pub fn is_map_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Empty maps and maps whose channels have no usable target both count as
    /// empty for the parent config's serde skip contract.
    pub fn is_empty(&self) -> bool {
        self.inner
            .values()
            .all(|channel| channel.target().is_none())
    }

    pub fn first_present(&self) -> Option<(&str, &AgentChannel)> {
        self.iter().next()
    }

    pub fn upsert<F>(&mut self, id: &str, update: F)
    where
        F: FnOnce(Option<AgentChannel>) -> Option<AgentChannel>,
    {
        if let Some(channel) = update(self.remove(id)) {
            self.insert(id, channel);
        }
    }
}
