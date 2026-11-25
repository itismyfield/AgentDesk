//! `discord.decisions`: where Decision forums live and what an ask names them. Discord IDs come
//! from config, never constants.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct DecisionsConfig {
    /// Category ID → forum ID; an entry replaces lookup and creation for that category.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub forums_by_category: BTreeMap<String, String>,
    /// The name an ask gives its category's Decision forum; `decision` when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forum_name: Option<String>,
}

impl DecisionsConfig {
    pub(crate) fn is_default(&self) -> bool {
        self.forums_by_category.is_empty() && self.forum_name.is_none()
    }

    pub(crate) fn forum_name(&self) -> &str {
        let name = self.forum_name.as_deref().map(str::trim);
        name.filter(|name| !name.is_empty()).unwrap_or("decision")
    }
}
