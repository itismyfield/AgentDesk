//! `discord.decisions`: where Decision forums live. Discord IDs come from config, never constants.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct DecisionsConfig {
    /// Category ID → forum ID; an entry replaces lookup and creation for that category.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub forums_by_category: BTreeMap<String, String>,
}

impl DecisionsConfig {
    pub(crate) fn is_default(&self) -> bool {
        self.forums_by_category.is_empty()
    }
}
