use serde::Serialize;
use std::path::{Path, PathBuf};

pub const OBSIDIAN_AGENT_PROMPTS_CONNECTOR: &str = "obsidian_agent_prompts";
pub const OBSIDIAN_SKILL_ROOT_CONNECTOR: &str = "obsidian_skill_root";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionalConnectorState {
    Ready,
    Skipped,
    MissingConfig,
    MissingPath,
    MissingProvider,
    InvalidConfig,
}

impl OptionalConnectorState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Skipped => "skipped",
            Self::MissingConfig => "missing_config",
            Self::MissingPath => "missing_path",
            Self::MissingProvider => "missing_provider",
            Self::InvalidConfig => "invalid_config",
        }
    }

    pub fn needs_operator_setup(self) -> bool {
        matches!(
            self,
            Self::MissingPath | Self::MissingProvider | Self::InvalidConfig
        )
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct OptionalConnectorStatus {
    pub id: &'static str,
    pub name: &'static str,
    pub state: OptionalConnectorState,
    pub optional: bool,
    pub env_var: &'static str,
    pub source: Option<String>,
    pub reason: Option<&'static str>,
    pub detail: String,
    pub setup_actions: Vec<String>,
    pub capabilities: Vec<&'static str>,
}

#[derive(Clone, Debug, Serialize)]
pub struct OptionalConnectorSummary {
    pub ready: usize,
    pub skipped: usize,
    pub missing_config: usize,
    pub missing_path: usize,
    pub missing_provider: usize,
    pub invalid_config: usize,
    pub invalid: usize,
    pub total: usize,
    pub core_runtime_blocking: bool,
}

impl OptionalConnectorSummary {
    pub fn from_statuses(statuses: &[OptionalConnectorStatus]) -> Self {
        let mut summary = Self {
            ready: 0,
            skipped: 0,
            missing_config: 0,
            missing_path: 0,
            missing_provider: 0,
            invalid_config: 0,
            invalid: 0,
            total: statuses.len(),
            core_runtime_blocking: false,
        };
        for status in statuses {
            match status.state {
                OptionalConnectorState::Ready => summary.ready += 1,
                OptionalConnectorState::Skipped => summary.skipped += 1,
                OptionalConnectorState::MissingConfig => summary.missing_config += 1,
                OptionalConnectorState::MissingPath => summary.missing_path += 1,
                OptionalConnectorState::MissingProvider => summary.missing_provider += 1,
                OptionalConnectorState::InvalidConfig => summary.invalid_config += 1,
            }
            if status.state.needs_operator_setup() {
                summary.invalid += 1;
            }
        }
        summary
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct OptionalConnectorsResponse {
    pub connectors: Vec<OptionalConnectorStatus>,
    pub summary: OptionalConnectorSummary,
}

impl OptionalConnectorsResponse {
    pub fn current() -> Self {
        let connectors = optional_connector_statuses();
        let summary = OptionalConnectorSummary::from_statuses(&connectors);
        Self {
            connectors,
            summary,
        }
    }
}

pub fn optional_connector_statuses() -> Vec<OptionalConnectorStatus> {
    vec![
        obsidian_agent_prompts_status(),
        obsidian_skill_root_status(),
    ]
}

pub fn optional_connector_status_by_id(id: &str) -> Option<OptionalConnectorStatus> {
    optional_connector_statuses().into_iter().find(|status| {
        status.id == id
            || status
                .capabilities
                .iter()
                .any(|capability| *capability == id)
    })
}

fn obsidian_agent_prompts_status() -> OptionalConnectorStatus {
    let source = explicit_env_path("AGENTDESK_OBSIDIAN_AGENTS_SRC").or_else(|| {
        obsidian_remote_vault_root().map(|root| root.join("adk-config").join("agents"))
    });
    connector_dir_status(ConnectorDirSpec {
        id: OBSIDIAN_AGENT_PROMPTS_CONNECTOR,
        name: "Obsidian agent prompts",
        env_var: "AGENTDESK_OBSIDIAN_AGENTS_SRC",
        source,
        explicit: explicit_env_path("AGENTDESK_OBSIDIAN_AGENTS_SRC").is_some(),
        capability: OBSIDIAN_AGENT_PROMPTS_CONNECTOR,
        setup_hint: "Set AGENTDESK_OBSIDIAN_AGENTS_SRC to an existing prompts directory, or run scripts/operator-init-portable.py --with-obsidian-stubs to create starter directories.",
    })
}

fn obsidian_skill_root_status() -> OptionalConnectorStatus {
    let source = explicit_env_path("AGENTDESK_OBSIDIAN_SKILL_ROOT")
        .or_else(|| obsidian_remote_vault_root().map(|root| root.join("99_Skills")));
    let mut status = connector_dir_status(ConnectorDirSpec {
        id: OBSIDIAN_SKILL_ROOT_CONNECTOR,
        name: "Obsidian skill root",
        env_var: "AGENTDESK_OBSIDIAN_SKILL_ROOT",
        source: source.clone(),
        explicit: explicit_env_path("AGENTDESK_OBSIDIAN_SKILL_ROOT").is_some(),
        capability: OBSIDIAN_SKILL_ROOT_CONNECTOR,
        setup_hint: "Set AGENTDESK_OBSIDIAN_SKILL_ROOT to an existing skill directory containing at least one <skill>/SKILL.md, or run scripts/operator-init-portable.py --with-obsidian-stubs before syncing real skills.",
    });
    if status.state == OptionalConnectorState::Ready
        && !source
            .as_deref()
            .is_some_and(obsidian_skill_root_contains_skill)
    {
        let source_text = status.source.clone().unwrap_or_default();
        status.state = OptionalConnectorState::InvalidConfig;
        status.reason = Some("missing_skill_files");
        status.detail =
            format!("state=invalid_config source={source_text} reason=missing_skill_files");
        status.setup_actions = vec![format!(
            "Add at least one skill directory containing SKILL.md under {source_text}."
        )];
    }
    status
}

struct ConnectorDirSpec {
    id: &'static str,
    name: &'static str,
    env_var: &'static str,
    source: Option<PathBuf>,
    explicit: bool,
    capability: &'static str,
    setup_hint: &'static str,
}

fn connector_dir_status(spec: ConnectorDirSpec) -> OptionalConnectorStatus {
    let Some(source) = spec.source else {
        return OptionalConnectorStatus {
            id: spec.id,
            name: spec.name,
            state: OptionalConnectorState::MissingConfig,
            optional: true,
            env_var: spec.env_var,
            source: None,
            reason: Some("home_unavailable"),
            detail: "state=missing_config reason=home_unavailable; core runtime does not require this connector".to_string(),
            setup_actions: vec![format!("Set {} to an existing directory.", spec.env_var)],
            capabilities: vec![spec.capability],
        };
    };

    let source_text = source.display().to_string();
    if source.is_dir() {
        OptionalConnectorStatus {
            id: spec.id,
            name: spec.name,
            state: OptionalConnectorState::Ready,
            optional: true,
            env_var: spec.env_var,
            source: Some(source_text.clone()),
            reason: None,
            detail: format!("state=ready source={source_text}"),
            setup_actions: Vec::new(),
            capabilities: vec![spec.capability],
        }
    } else if spec.explicit {
        OptionalConnectorStatus {
            id: spec.id,
            name: spec.name,
            state: OptionalConnectorState::MissingPath,
            optional: true,
            env_var: spec.env_var,
            source: Some(source_text.clone()),
            reason: Some("missing_path"),
            detail: format!("state=missing_path source={source_text} reason=missing_path"),
            setup_actions: vec![
                format!("Create the configured directory: {source_text}"),
                format!(
                    "Unset {} or point it at an existing directory.",
                    spec.env_var
                ),
            ],
            capabilities: vec![spec.capability],
        }
    } else {
        OptionalConnectorStatus {
            id: spec.id,
            name: spec.name,
            state: OptionalConnectorState::MissingConfig,
            optional: true,
            env_var: spec.env_var,
            source: Some(source_text.clone()),
            reason: Some("missing_config"),
            detail: format!("state=missing_config source={source_text} reason=missing_config"),
            setup_actions: vec![spec.setup_hint.to_string()],
            capabilities: vec![spec.capability],
        }
    }
}

fn obsidian_skill_root_contains_skill(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let path = entry.path();
        path.is_dir() && path.join("SKILL.md").is_file()
    })
}

fn explicit_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn obsidian_remote_vault_root() -> Option<PathBuf> {
    explicit_env_path("OBSIDIAN_REMOTE_VAULT_ROOT").or_else(|| {
        explicit_env_path("OBSIDIAN_VAULT_ROOT")
            .map(|root| root.join("RemoteVault"))
            .or_else(runtime_root_obsidian_remote_vault_root)
            .or_else(|| {
                operator_home_dir().map(|home| home.join("ObsidianVault").join("RemoteVault"))
            })
    })
}

fn runtime_root_obsidian_remote_vault_root() -> Option<PathBuf> {
    let remote = crate::config::runtime_root()?
        .join("ObsidianVault")
        .join("RemoteVault");
    remote.exists().then_some(remote)
}

fn operator_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .or_else(dirs::home_dir)
}

#[cfg(test)]
mod tests {
    use super::{
        OptionalConnectorState, OptionalConnectorSummary, optional_connector_status_by_id,
        optional_connector_statuses,
    };
    fn with_connector_env<F>(f: F)
    where
        F: FnOnce(),
    {
        let _guard = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let saved_home = std::env::var_os("HOME");
        let saved_userprofile = std::env::var_os("USERPROFILE");
        let saved_obsidian_root = std::env::var_os("OBSIDIAN_VAULT_ROOT");
        let saved_remote_root = std::env::var_os("OBSIDIAN_REMOTE_VAULT_ROOT");
        let saved_agents_src = std::env::var_os("AGENTDESK_OBSIDIAN_AGENTS_SRC");
        let saved_skill_root = std::env::var_os("AGENTDESK_OBSIDIAN_SKILL_ROOT");
        let saved_runtime_root = std::env::var_os("AGENTDESK_ROOT_DIR");

        f();

        restore_env("HOME", saved_home);
        restore_env("USERPROFILE", saved_userprofile);
        restore_env("OBSIDIAN_VAULT_ROOT", saved_obsidian_root);
        restore_env("OBSIDIAN_REMOTE_VAULT_ROOT", saved_remote_root);
        restore_env("AGENTDESK_OBSIDIAN_AGENTS_SRC", saved_agents_src);
        restore_env("AGENTDESK_OBSIDIAN_SKILL_ROOT", saved_skill_root);
        restore_env("AGENTDESK_ROOT_DIR", saved_runtime_root);
    }

    fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
        match value {
            Some(value) => unsafe { std::env::set_var(name, value) },
            None => unsafe { std::env::remove_var(name) },
        }
    }

    #[test]
    fn connector_lookup_matches_capability_alias() {
        let status = optional_connector_status_by_id("obsidian_skill_root")
            .expect("known connector should resolve");
        assert_eq!(status.id, "obsidian_skill_root");
    }

    #[test]
    fn connector_state_labels_are_api_stable() {
        assert_eq!(OptionalConnectorState::Ready.as_str(), "ready");
        assert_eq!(OptionalConnectorState::Skipped.as_str(), "skipped");
        assert_eq!(
            OptionalConnectorState::MissingConfig.as_str(),
            "missing_config"
        );
        assert_eq!(OptionalConnectorState::MissingPath.as_str(), "missing_path");
        assert_eq!(
            OptionalConnectorState::MissingProvider.as_str(),
            "missing_provider"
        );
        assert_eq!(
            OptionalConnectorState::InvalidConfig.as_str(),
            "invalid_config"
        );
    }

    #[test]
    fn implicit_missing_connector_reports_missing_config() {
        with_connector_env(|| {
            let temp = tempfile::tempdir().unwrap();
            unsafe {
                std::env::set_var("HOME", temp.path());
                std::env::set_var("USERPROFILE", temp.path());
                std::env::remove_var("OBSIDIAN_VAULT_ROOT");
                std::env::remove_var("OBSIDIAN_REMOTE_VAULT_ROOT");
                std::env::remove_var("AGENTDESK_OBSIDIAN_AGENTS_SRC");
                std::env::remove_var("AGENTDESK_OBSIDIAN_SKILL_ROOT");
            }

            let statuses = optional_connector_statuses();
            assert!(
                statuses
                    .iter()
                    .all(|status| status.state == OptionalConnectorState::MissingConfig)
            );
            let summary = OptionalConnectorSummary::from_statuses(&statuses);
            assert_eq!(summary.missing_config, statuses.len());
            assert_eq!(summary.invalid, 0);
        });
    }

    #[test]
    fn explicit_missing_connector_reports_missing_path() {
        with_connector_env(|| {
            let temp = tempfile::tempdir().unwrap();
            let missing = temp.path().join("missing-agents");
            unsafe {
                std::env::set_var("HOME", temp.path());
                std::env::set_var("USERPROFILE", temp.path());
                std::env::set_var("AGENTDESK_OBSIDIAN_AGENTS_SRC", &missing);
                std::env::remove_var("OBSIDIAN_VAULT_ROOT");
                std::env::remove_var("OBSIDIAN_REMOTE_VAULT_ROOT");
                std::env::remove_var("AGENTDESK_OBSIDIAN_SKILL_ROOT");
            }

            let status = optional_connector_status_by_id("obsidian_agent_prompts").unwrap();
            assert_eq!(status.state, OptionalConnectorState::MissingPath);
            assert_eq!(status.reason, Some("missing_path"));
            let summary = OptionalConnectorSummary::from_statuses(&[status]);
            assert_eq!(summary.missing_path, 1);
            assert_eq!(summary.invalid, 1);
        });
    }

    #[test]
    fn runtime_root_obsidian_stubs_are_discoverable_without_obsidian_env() {
        with_connector_env(|| {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("home");
            let root = temp.path().join("release");
            let remote = root.join("ObsidianVault").join("RemoteVault");
            std::fs::create_dir_all(remote.join("adk-config").join("agents")).unwrap();
            let skill_root = remote.join("99_Skills");
            std::fs::create_dir_all(skill_root.join("ai-integrated-briefing")).unwrap();
            std::fs::write(
                skill_root.join("ai-integrated-briefing").join("SKILL.md"),
                "# AI integrated briefing\n",
            )
            .unwrap();
            unsafe {
                std::env::set_var("HOME", &home);
                std::env::set_var("USERPROFILE", &home);
                std::env::set_var("AGENTDESK_ROOT_DIR", &root);
                std::env::remove_var("OBSIDIAN_VAULT_ROOT");
                std::env::remove_var("OBSIDIAN_REMOTE_VAULT_ROOT");
                std::env::remove_var("AGENTDESK_OBSIDIAN_AGENTS_SRC");
                std::env::remove_var("AGENTDESK_OBSIDIAN_SKILL_ROOT");
            }

            let statuses = optional_connector_statuses();

            assert!(
                statuses
                    .iter()
                    .all(|status| status.state == OptionalConnectorState::Ready)
            );
            assert!(statuses.iter().all(|status| {
                status
                    .source
                    .as_deref()
                    .is_some_and(|source| source.contains("ObsidianVault"))
            }));
        });
    }

    #[test]
    fn empty_obsidian_skill_stub_requires_real_skill_content() {
        with_connector_env(|| {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("home");
            let root = temp.path().join("release");
            let remote = root.join("ObsidianVault").join("RemoteVault");
            std::fs::create_dir_all(remote.join("adk-config").join("agents")).unwrap();
            std::fs::create_dir_all(remote.join("99_Skills")).unwrap();
            unsafe {
                std::env::set_var("HOME", &home);
                std::env::set_var("USERPROFILE", &home);
                std::env::set_var("AGENTDESK_ROOT_DIR", &root);
                std::env::remove_var("OBSIDIAN_VAULT_ROOT");
                std::env::remove_var("OBSIDIAN_REMOTE_VAULT_ROOT");
                std::env::remove_var("AGENTDESK_OBSIDIAN_AGENTS_SRC");
                std::env::remove_var("AGENTDESK_OBSIDIAN_SKILL_ROOT");
            }

            let prompts = optional_connector_status_by_id("obsidian_agent_prompts").unwrap();
            let skills = optional_connector_status_by_id("obsidian_skill_root").unwrap();

            assert_eq!(prompts.state, OptionalConnectorState::Ready);
            assert_eq!(skills.state, OptionalConnectorState::InvalidConfig);
            assert_eq!(skills.reason, Some("missing_skill_files"));
        });
    }
}
