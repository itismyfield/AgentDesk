use super::*;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct AgentDef {
    pub id: String,
    /// Shared Memento workspace; absent or blank retains the legacy cwd scope.
    #[serde(
        default,
        deserialize_with = "deserialize_memory_group",
        skip_serializing_if = "Option::is_none"
    )]
    pub memory_group: Option<String>,
    pub name: String,
    #[serde(default)]
    pub name_ko: Option<String>,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "AgentChannels::is_empty")]
    pub channels: AgentChannels,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    /// Peer discovery terms, separate from `keywords`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peer_keywords: Vec<String>,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub peer_directory: bool,
    #[serde(default)]
    pub department: Option<String>,
    #[serde(default)]
    pub avatar_emoji: Option<String>,
    /// Cluster intake node-affinity labels (#3667). When set and non-empty,
    /// intake for this agent's channels is routed to an online worker node whose
    /// labels satisfy this list; `Some([])` means no preference (intake stays on
    /// the leader). `None` (key absent from yaml) leaves any existing DB value
    /// untouched on sync, so an out-of-band label is never wiped. Maps to the
    /// `agents.preferred_intake_node_labels` JSONB column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_intake_node_labels: Option<Vec<String>>,
}

/// `memory.shared_knowledge_injection`: whether the SAK file goes into agent prompts.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SharedKnowledgeInjection {
    /// Inject only when the agents lack the Memento MCP.
    #[default]
    Auto,
    On,
    Off,
}

impl SharedKnowledgeInjection {
    pub fn is_auto(&self) -> bool {
        *self == Self::Auto
    }
}

impl Config {
    /// Memento is available when the MCP catalog handed to agents (yaml, or the
    /// default server added when `MEMENTO_ACCESS_KEY` is set) names `memento` with a URL.
    pub(crate) fn memento_available(&self) -> bool {
        self.mcp_servers
            .iter()
            .any(|(name, server)| name.trim() == "memento" && !server.url.trim().is_empty())
    }

    /// The SAK file is the memory fallback for deployments without Memento.
    pub(crate) fn shared_knowledge_injection_enabled(&self) -> bool {
        match self.memory.as_ref().map(|m| m.shared_knowledge_injection) {
            Some(SharedKnowledgeInjection::On) => true,
            Some(SharedKnowledgeInjection::Off) => false,
            Some(SharedKnowledgeInjection::Auto) | None => !self.memento_available(),
        }
    }
}

fn deserialize_memory_group<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?
        .map(|group| group.trim().to_owned())
        .filter(|group| !group.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_group_parsing_and_round_trip_preserve_old_formats() {
        for (field, expected) in [
            ("", None),
            ("memory_group: ''", None),
            ("memory_group: '   '", None),
            ("memory_group: null", None),
            ("memory_group: ' cookingheart '", Some("cookingheart")),
        ] {
            let agent: AgentDef = serde_yaml::from_str(&format!(
                "id: ch-td\nname: Test\nfuture_key: ignored\n{field}\n"
            ))
            .unwrap();
            assert_eq!(agent.memory_group.as_deref(), expected);
            let yaml = serde_yaml::to_string(&agent).unwrap();
            assert_eq!(yaml.contains("memory_group:"), expected.is_some());
            assert_eq!(serde_yaml::from_str::<AgentDef>(&yaml).unwrap(), agent);
        }
        assert!(
            serde_yaml::from_str::<AgentDef>("id: ch-td\nname: Test\nmemory_group: []").is_err()
        );
    }
}
