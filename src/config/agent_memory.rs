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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake_word: Option<String>,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub voice_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensitivity_mode: Option<BargeInSensitivity>,
    #[serde(default, skip_serializing_if = "AgentVoiceConfig::is_default")]
    pub voice: AgentVoiceConfig,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default, skip_serializing_if = "AgentChannels::is_empty")]
    pub channels: AgentChannels,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub department: Option<String>,
    #[serde(default)]
    pub avatar_emoji: Option<String>,
    /// Optional cluster intake node labels; omission preserves the DB value on sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_intake_node_labels: Option<Vec<String>>,
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
