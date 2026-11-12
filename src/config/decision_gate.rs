use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct DecisionGateConfig {
    pub enabled: bool,
    pub features: Features,
    #[serde(deserialize_with = "repositories")]
    pub repositories: BTreeMap<String, Repository>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Features {
    pub issue_duplicate: FeaturePolicy<5000>,
    pub host_assignment: FeaturePolicy<2000>,
    pub routine_precheck: FeaturePolicy<2000>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct FeaturePolicy<const DEFAULT_DEADLINE_MS: u64> {
    pub enabled: bool,
    pub deadline_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct Repository {
    pub enabled: bool,
}

impl Default for DecisionGateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            features: Features::default(),
            repositories: BTreeMap::from([
                ("itismyfield/agentdesk".into(), Repository { enabled: true }),
                (
                    "itismyfield/cookingheart".into(),
                    Repository { enabled: false },
                ),
            ]),
        }
    }
}

impl Default for Features {
    fn default() -> Self {
        Self {
            issue_duplicate: FeaturePolicy::default(),
            host_assignment: FeaturePolicy::default(),
            routine_precheck: FeaturePolicy::default(),
        }
    }
}

impl<const DEFAULT_DEADLINE_MS: u64> Default for FeaturePolicy<DEFAULT_DEADLINE_MS> {
    fn default() -> Self {
        Self {
            enabled: true,
            deadline_ms: DEFAULT_DEADLINE_MS,
        }
    }
}

impl DecisionGateConfig {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        for deadline in [
            self.features.issue_duplicate.deadline_ms,
            self.features.host_assignment.deadline_ms,
            self.features.routine_precheck.deadline_ms,
        ] {
            anyhow::ensure!(
                (1..=5000).contains(&deadline),
                "decision_gate deadline_ms must be 1..=5000"
            );
        }
        let mut normalized = std::collections::BTreeSet::new();
        for key in self.repositories.keys() {
            let key = normalize_repository(key).ok_or_else(|| {
                anyhow::anyhow!("decision_gate repository must be ADK, CH, or owner/repo")
            })?;
            anyhow::ensure!(
                normalized.insert(key),
                "duplicate decision_gate repository after normalization"
            );
        }
        Ok(())
    }
}

pub(crate) fn normalize_repository(raw: &str) -> Option<String> {
    let value = raw.trim().to_ascii_lowercase();
    let value = match value.as_str() {
        "adk" => "itismyfield/agentdesk",
        "ch" => "itismyfield/cookingheart",
        _ => &value,
    };
    let (owner, repo) = value.split_once('/')?;
    (!owner.is_empty()
        && !repo.is_empty()
        && !repo.contains('/')
        && !value.chars().any(char::is_whitespace))
    .then(|| value.to_owned())
}

fn repositories<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, Repository>, D::Error> {
    use serde::de::{Error, MapAccess, Visitor};
    struct Repositories;
    impl<'de> Visitor<'de> for Repositories {
        type Value = BTreeMap<String, Repository>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("unique decision_gate repositories")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut values = DecisionGateConfig::default().repositories;
            let mut seen = std::collections::BTreeSet::new();
            while let Some((raw, policy)) = map.next_entry::<String, Repository>()? {
                let key = normalize_repository(&raw)
                    .ok_or_else(|| M::Error::custom("invalid decision_gate repository"))?;
                if !seen.insert(key.clone()) {
                    return Err(M::Error::custom(
                        "duplicate decision_gate repository after normalization",
                    ));
                }
                values.insert(key, policy);
            }
            Ok(values)
        }
    }
    deserializer.deserialize_map(Repositories)
}
