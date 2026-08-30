//! Named provider CLI auth profiles: catalog, overlay resolver, spawn env.
//!
//! Unset `auth_profile` is the implicit `default` overlay (current global home).
//! Named ids fail-closed: no silent fallback to the global home.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::services::provider::ProviderKind;
use crate::utils::format::expand_tilde_string as expand_tilde;

pub const DEFAULT_PROFILE_ID: &str = "default";

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProviderAuthProfileDef {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ProviderAuthOverlay {
    pub profile_id: String,
    pub provider: ProviderKind,
    pub env: BTreeMap<String, String>,
    pub unset: BTreeSet<String>,
    pub home: Option<PathBuf>,
}

impl ProviderAuthOverlay {
    pub fn default_for(provider: ProviderKind) -> Self {
        Self {
            profile_id: DEFAULT_PROFILE_ID.to_string(),
            provider,
            env: BTreeMap::new(),
            unset: BTreeSet::new(),
            home: None,
        }
    }

    pub fn is_default(&self) -> bool {
        self.profile_id == DEFAULT_PROFILE_ID && self.env.is_empty() && self.unset.is_empty()
    }
}

impl fmt::Debug for ProviderAuthOverlay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted: BTreeMap<&String, &'static str> =
            self.env.keys().map(|key| (key, "<redacted>")).collect();
        f.debug_struct("ProviderAuthOverlay")
            .field("profile_id", &self.profile_id)
            .field("provider", &self.provider.as_str())
            .field("env", &redacted)
            .field("unset", &self.unset)
            .field("home", &self.home)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthProfileError {
    InvalidProfileId(String),
    ReservedProfileId,
    UnknownProvider(String),
    UnknownProfile(String),
    ProviderMismatch {
        profile_id: String,
        expected: String,
        actual: String,
    },
    HomeRequired {
        profile_id: String,
        provider: String,
    },
    HomeMissing {
        profile_id: String,
        home: String,
    },
    UnsupportedLogin(String),
    GlobalHomeForbidden {
        provider: String,
        home: String,
    },
    AlreadyCataloged(String),
    CredentialsMissing {
        profile_id: String,
        home: String,
    },
    Io(String),
}

impl fmt::Display for AuthProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProfileId(id) => write!(f, "invalid auth_profile id '{id}'"),
            Self::ReservedProfileId => {
                write!(
                    f,
                    "auth_profile id 'default' is reserved and must not be cataloged"
                )
            }
            Self::UnknownProvider(provider) => {
                write!(f, "unknown provider '{provider}' in provider_auth_profiles")
            }
            Self::UnknownProfile(id) => write!(f, "unknown auth_profile '{id}'"),
            Self::ProviderMismatch {
                profile_id,
                expected,
                actual,
            } => write!(
                f,
                "auth_profile '{profile_id}' provider '{actual}' does not match turn provider '{expected}'"
            ),
            Self::HomeRequired {
                profile_id,
                provider,
            } => write!(
                f,
                "auth_profile '{profile_id}' for {provider} requires home"
            ),
            Self::HomeMissing { profile_id, home } => write!(
                f,
                "auth_profile '{profile_id}' home '{home}' does not exist"
            ),
            Self::UnsupportedLogin(provider) => {
                write!(f, "extra-account login is not supported for '{provider}'")
            }
            Self::GlobalHomeForbidden { provider, home } => write!(
                f,
                "refusing to use global {provider} home '{home}' as an extra account"
            ),
            Self::AlreadyCataloged(id) => write!(f, "auth_profile '{id}' already exists"),
            Self::CredentialsMissing { profile_id, home } => write!(
                f,
                "auth_profile '{profile_id}' home '{home}' has no provider credentials yet"
            ),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for AuthProfileError {}

pub fn named_profile_requires_home(provider: &ProviderKind) -> bool {
    matches!(
        provider,
        ProviderKind::Claude
            | ProviderKind::Codex
            | ProviderKind::Qwen
            | ProviderKind::OpenCode
            | ProviderKind::Grok
    )
}

pub fn home_env_for(provider: &ProviderKind, home: &Path) -> BTreeMap<String, String> {
    let home = home.to_string_lossy().into_owned();
    let mut env = BTreeMap::new();
    match provider {
        ProviderKind::Claude => {
            env.insert("CLAUDE_CONFIG_DIR".to_string(), home);
        }
        ProviderKind::Codex => {
            env.insert("CODEX_HOME".to_string(), home);
        }
        ProviderKind::Qwen => {
            env.insert("QWEN_HOME".to_string(), home);
        }
        ProviderKind::OpenCode => {
            env.insert("XDG_CONFIG_HOME".to_string(), format!("{home}/xdg-config"));
            env.insert("XDG_DATA_HOME".to_string(), format!("{home}/xdg-data"));
        }
        ProviderKind::Grok => {
            env.insert("GROK_HOME".to_string(), home);
        }
        ProviderKind::Antigravity | ProviderKind::Gemini | ProviderKind::Unsupported(_) => {}
    }
    env
}

pub fn detect_relative_paths(provider: &ProviderKind) -> &'static [&'static str] {
    match provider {
        ProviderKind::Claude => &[".credentials.json"],
        ProviderKind::Codex | ProviderKind::Grok => &["auth.json"],
        ProviderKind::Qwen => &["oauth_creds.json", "settings.json", ".env"],
        ProviderKind::OpenCode => &[
            "xdg-config/opencode/opencode.json",
            "xdg-data/opencode/auth.json",
        ],
        _ => &[],
    }
}

pub fn validate_profile_id(id: &str) -> Result<(), AuthProfileError> {
    if id == DEFAULT_PROFILE_ID {
        return Err(AuthProfileError::ReservedProfileId);
    }
    let valid = id.len() <= 64
        && id.chars().next().is_some_and(|ch| ch.is_ascii_lowercase())
        && id
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-');
    if valid {
        Ok(())
    } else {
        Err(AuthProfileError::InvalidProfileId(id.to_string()))
    }
}

pub fn intern_provider(raw: &str) -> Result<ProviderKind, AuthProfileError> {
    ProviderKind::from_str(raw).ok_or_else(|| AuthProfileError::UnknownProvider(raw.to_string()))
}

pub fn validate_catalog(
    catalog: &HashMap<String, ProviderAuthProfileDef>,
) -> Result<(), AuthProfileError> {
    for (id, def) in catalog {
        validate_profile_id(id)?;
        let provider = intern_provider(&def.provider)?;
        if named_profile_requires_home(&provider) {
            let home = def.home.as_deref().map(str::trim).unwrap_or("");
            if home.is_empty() {
                return Err(AuthProfileError::HomeRequired {
                    profile_id: id.clone(),
                    provider: provider.as_str().to_string(),
                });
            }
        }
    }
    Ok(())
}

pub fn selected_profile_id<'a>(
    channel_auth_profile: Option<&'a str>,
    agent_auth_profile: Option<&'a str>,
) -> Option<&'a str> {
    channel_auth_profile
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            agent_auth_profile
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .filter(|value| *value != DEFAULT_PROFILE_ID)
}

pub fn resolve(
    provider: ProviderKind,
    channel_auth_profile: Option<&str>,
    agent_auth_profile: Option<&str>,
    catalog: &HashMap<String, ProviderAuthProfileDef>,
) -> Result<ProviderAuthOverlay, AuthProfileError> {
    let Some(profile_id) = selected_profile_id(channel_auth_profile, agent_auth_profile) else {
        return Ok(ProviderAuthOverlay::default_for(provider));
    };
    let def = catalog
        .get(profile_id)
        .ok_or_else(|| AuthProfileError::UnknownProfile(profile_id.to_string()))?;
    let catalog_provider = intern_provider(&def.provider)?;
    if catalog_provider.as_str() != provider.as_str() {
        return Err(AuthProfileError::ProviderMismatch {
            profile_id: profile_id.to_string(),
            expected: provider.as_str().to_string(),
            actual: catalog_provider.as_str().to_string(),
        });
    }

    let home = def
        .home
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(expand_tilde)
        .map(PathBuf::from);

    if named_profile_requires_home(&provider) {
        let Some(home_path) = home.as_ref() else {
            return Err(AuthProfileError::HomeRequired {
                profile_id: profile_id.to_string(),
                provider: provider.as_str().to_string(),
            });
        };
        if !home_path.is_dir() {
            return Err(AuthProfileError::HomeMissing {
                profile_id: profile_id.to_string(),
                home: home_path.display().to_string(),
            });
        }
    }

    let mut env = home
        .as_ref()
        .map(|path| home_env_for(&provider, path))
        .unwrap_or_default();
    let mut unset = BTreeSet::new();
    for (key, value) in &def.env {
        if value.is_empty() {
            env.remove(key);
            unset.insert(key.clone());
        } else {
            unset.remove(key);
            env.insert(key.clone(), value.clone());
        }
    }

    Ok(ProviderAuthOverlay {
        profile_id: profile_id.to_string(),
        provider,
        env,
        unset,
        home,
    })
}

pub fn merge_overlay_env(
    base: Vec<(String, String)>,
    overlay: &ProviderAuthOverlay,
) -> Vec<(String, String)> {
    let mut map: BTreeMap<String, String> = base.into_iter().collect();
    for key in &overlay.unset {
        map.remove(key);
    }
    for (key, value) in &overlay.env {
        map.insert(key.clone(), value.clone());
    }
    map.into_iter().collect()
}

pub fn apply_overlay_to_command(command: &mut Command, overlay: &ProviderAuthOverlay) {
    for key in &overlay.unset {
        command.env_remove(key);
    }
    for (key, value) in &overlay.env {
        command.env(key, value);
    }
}

pub fn overlay_env_pairs(overlay: &ProviderAuthOverlay) -> Vec<(String, String)> {
    overlay
        .env
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

pub fn overlay_unset_keys(overlay: &ProviderAuthOverlay) -> Vec<String> {
    overlay.unset.iter().cloned().collect()
}

pub const EXTRA_ACCOUNT_PROVIDER_IDS: &[&str] = &["claude", "codex", "qwen", "opencode", "grok"];

pub fn extra_account_login_supported(provider: &ProviderKind) -> bool {
    EXTRA_ACCOUNT_PROVIDER_IDS
        .iter()
        .any(|id| *id == provider.as_str())
}

pub fn default_home_display(provider: &ProviderKind) -> Option<&'static str> {
    match provider {
        ProviderKind::Claude => Some("~/.claude"),
        ProviderKind::Codex => Some("~/.codex"),
        ProviderKind::Qwen => Some("~/.qwen"),
        ProviderKind::Grok => Some("~/.grok"),
        ProviderKind::OpenCode => Some("~/.local/share/opencode"),
        _ => None,
    }
}

pub fn default_home_path(provider: &ProviderKind) -> Option<PathBuf> {
    default_home_display(provider)
        .map(expand_tilde)
        .map(PathBuf::from)
}

pub fn extra_accounts_root() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".adk")
        .join("profiles")
}

pub fn extra_account_home(provider: &ProviderKind, profile_id: &str) -> PathBuf {
    extra_account_home_at(&extra_accounts_root(), provider, profile_id)
}

pub fn extra_account_home_at(root: &Path, provider: &ProviderKind, profile_id: &str) -> PathBuf {
    root.join(provider.as_str()).join(profile_id)
}

pub fn compact_home_for_yaml(path: &Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(stripped) = path.strip_prefix(&home) {
            return format!("~/{}", stripped.display());
        }
    }
    path.display().to_string()
}

pub fn overlay_for_home(
    provider: ProviderKind,
    profile_id: &str,
    home: &Path,
) -> ProviderAuthOverlay {
    ProviderAuthOverlay {
        profile_id: profile_id.to_string(),
        provider: provider.clone(),
        env: home_env_for(&provider, home),
        unset: BTreeSet::new(),
        home: Some(home.to_path_buf()),
    }
}

pub fn vendor_login_argv(provider: &ProviderKind) -> Option<&'static [&'static str]> {
    match provider {
        // Orca extra-account login: temp CLAUDE_CONFIG_DIR + `claude auth login --claudeai`.
        ProviderKind::Claude => Some(&["claude", "auth", "login", "--claudeai"]),
        // Orca uses device-auth so extra login works without a loopback browser on SSH/tmux.
        ProviderKind::Codex => Some(&["codex", "login", "--device-auth"]),
        ProviderKind::Grok => Some(&["grok", "login", "--device-auth"]),
        ProviderKind::Qwen => Some(&["qwen"]),
        ProviderKind::OpenCode => Some(&["opencode", "auth", "login"]),
        _ => None,
    }
}

pub fn login_tmux_session_name(provider: &ProviderKind, profile_id: &str) -> String {
    format!("adk-login-{}-{}", provider.as_str(), profile_id)
}

pub fn named_claude_profile_ids(catalog: &HashMap<String, ProviderAuthProfileDef>) -> Vec<String> {
    let mut ids: Vec<String> = catalog
        .iter()
        .filter_map(|(id, def)| {
            intern_provider(&def.provider)
                .ok()
                .filter(|provider| matches!(provider, ProviderKind::Claude))
                .map(|_| id.clone())
        })
        .collect();
    ids.sort();
    ids
}

pub fn allocate_profile_id(
    provider: &ProviderKind,
    catalog: &HashMap<String, ProviderAuthProfileDef>,
    existing_homes: &[PathBuf],
) -> Result<String, AuthProfileError> {
    let base = format!("{}-alt", provider.as_str());
    let mut candidates = vec![base.clone()];
    for index in 2..64 {
        candidates.push(format!("{base}-{index}"));
    }
    for candidate in candidates {
        validate_profile_id(&candidate)?;
        if catalog.contains_key(&candidate) {
            continue;
        }
        let home = extra_account_home(provider, &candidate);
        if existing_homes.iter().any(|path| path == &home) {
            continue;
        }
        return Ok(candidate);
    }
    Err(AuthProfileError::InvalidProfileId(base))
}

/// Create an empty isolated extra-account home. Never copies the global home.
pub fn create_empty_profile_home(
    provider: &ProviderKind,
    profile_id: &str,
) -> Result<PathBuf, AuthProfileError> {
    create_empty_profile_home_at(&extra_accounts_root(), provider, profile_id)
}

pub fn create_empty_profile_home_at(
    root: &Path,
    provider: &ProviderKind,
    profile_id: &str,
) -> Result<PathBuf, AuthProfileError> {
    validate_profile_id(profile_id)?;
    if !extra_account_login_supported(provider) {
        return Err(AuthProfileError::UnsupportedLogin(
            provider.as_str().to_string(),
        ));
    }
    let home = extra_account_home_at(root, provider, profile_id);
    if let Some(global) = default_home_path(provider) {
        if same_path(&home, &global) {
            return Err(AuthProfileError::GlobalHomeForbidden {
                provider: provider.as_str().to_string(),
                home: home.display().to_string(),
            });
        }
    }
    if home.is_file() {
        return Err(AuthProfileError::Io(format!(
            "profile home '{}' exists as a file",
            home.display()
        )));
    }
    std::fs::create_dir_all(&home).map_err(|error| {
        AuthProfileError::Io(format!("create profile home '{}': {error}", home.display()))
    })?;
    if matches!(provider, ProviderKind::OpenCode) {
        for child in ["xdg-config", "xdg-data"] {
            std::fs::create_dir_all(home.join(child)).map_err(|error| {
                AuthProfileError::Io(format!(
                    "create OpenCode {child} under '{}': {error}",
                    home.display()
                ))
            })?;
        }
    }
    Ok(home)
}

pub fn login_script_contents(overlay: &ProviderAuthOverlay, argv: &[&str]) -> String {
    let mut body = String::from("#!/bin/bash\nset -euo pipefail\n");
    for (key, value) in &overlay.env {
        body.push_str("export ");
        body.push_str(key);
        body.push('=');
        body.push_str(&crate::services::process::shell_escape(value));
        body.push('\n');
    }
    for key in &overlay.unset {
        body.push_str("unset ");
        body.push_str(key);
        body.push('\n');
    }
    let command = argv
        .iter()
        .map(|arg| crate::services::process::shell_escape(arg))
        .collect::<Vec<_>>()
        .join(" ");
    body.push_str("exec ");
    body.push_str(&command);
    body.push('\n');
    body
}

pub fn write_login_script(
    home: &Path,
    overlay: &ProviderAuthOverlay,
    argv: &[&str],
) -> Result<PathBuf, AuthProfileError> {
    let path = home.join(".adk-login.sh");
    let body = login_script_contents(overlay, argv);
    std::fs::write(&path, body).map_err(|error| {
        AuthProfileError::Io(format!("write login script '{}': {error}", path.display()))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&path)
            .map_err(|error| {
                AuthProfileError::Io(format!("stat login script '{}': {error}", path.display()))
            })?
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).map_err(|error| {
            AuthProfileError::Io(format!("chmod login script '{}': {error}", path.display()))
        })?;
    }
    Ok(path)
}

fn same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (std::fs::canonicalize(left), std::fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn catalog_with(
        id: &str,
        def: ProviderAuthProfileDef,
    ) -> HashMap<String, ProviderAuthProfileDef> {
        let mut catalog = HashMap::new();
        catalog.insert(id.to_string(), def);
        catalog
    }

    fn temp_home() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp home");
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    #[test]
    fn test_001_unset_yields_default_overlay() {
        let overlay = resolve(ProviderKind::Codex, None, None, &HashMap::new()).unwrap();
        assert!(overlay.is_default());
        assert_eq!(overlay.profile_id, "default");
        assert!(overlay.env.is_empty());
        let merged = merge_overlay_env(vec![("PATH".into(), "/bin".into())], &overlay);
        assert_eq!(merged, vec![("PATH".into(), "/bin".into())]);
    }

    #[test]
    fn test_002_named_profiles_inject_home_env_without_home_rewrite() {
        let (_dir, home) = temp_home();
        let home_s = home.display().to_string();
        let cases: Vec<(ProviderKind, &str, Vec<(&str, String)>)> = vec![
            (
                ProviderKind::Codex,
                "work",
                vec![("CODEX_HOME", home_s.clone())],
            ),
            (
                ProviderKind::Qwen,
                "qwen-b",
                vec![("QWEN_HOME", home_s.clone())],
            ),
            (
                ProviderKind::Claude,
                "claude-b",
                vec![("CLAUDE_CONFIG_DIR", home_s.clone())],
            ),
            (
                ProviderKind::OpenCode,
                "oc-alt",
                vec![
                    ("XDG_CONFIG_HOME", format!("{home_s}/xdg-config")),
                    ("XDG_DATA_HOME", format!("{home_s}/xdg-data")),
                ],
            ),
            (
                ProviderKind::Grok,
                "grok-alt",
                vec![("GROK_HOME", home_s.clone())],
            ),
        ];
        for (provider, id, expected) in cases {
            let overlay = resolve(
                provider.clone(),
                Some(id),
                None,
                &catalog_with(
                    id,
                    ProviderAuthProfileDef {
                        provider: provider.as_str().to_string(),
                        home: Some(home_s.clone()),
                        env: BTreeMap::new(),
                    },
                ),
            )
            .unwrap();
            assert!(!overlay.env.contains_key("HOME"));
            for (key, value) in expected {
                assert_eq!(overlay.env.get(key), Some(&value), "{id} {key}");
            }
        }
    }

    #[test]
    fn test_003_channel_overrides_agent() {
        let (_dir, home) = temp_home();
        let mut catalog = HashMap::new();
        catalog.insert(
            "work".to_string(),
            ProviderAuthProfileDef {
                provider: "codex".into(),
                home: Some(home.display().to_string()),
                env: BTreeMap::new(),
            },
        );
        catalog.insert(
            "other".to_string(),
            ProviderAuthProfileDef {
                provider: "codex".into(),
                home: Some(home.display().to_string()),
                env: BTreeMap::new(),
            },
        );
        let overlay = resolve(ProviderKind::Codex, Some("work"), Some("other"), &catalog).unwrap();
        assert_eq!(overlay.profile_id, "work");
    }

    #[test]
    fn test_004_unknown_mismatch_missing_home_fail_closed() {
        let err = resolve(ProviderKind::Codex, Some("missing"), None, &HashMap::new()).unwrap_err();
        assert!(matches!(err, AuthProfileError::UnknownProfile(_)));

        let (_dir, home) = temp_home();
        let catalog = catalog_with(
            "work",
            ProviderAuthProfileDef {
                provider: "qwen".into(),
                home: Some(home.display().to_string()),
                env: BTreeMap::new(),
            },
        );
        let err = resolve(ProviderKind::Codex, Some("work"), None, &catalog).unwrap_err();
        assert!(matches!(err, AuthProfileError::ProviderMismatch { .. }));

        let catalog = catalog_with(
            "work",
            ProviderAuthProfileDef {
                provider: "codex".into(),
                home: Some("/definitely-not-a-real-auth-profile-home".into()),
                env: BTreeMap::new(),
            },
        );
        let err = resolve(ProviderKind::Codex, Some("work"), None, &catalog).unwrap_err();
        assert!(matches!(err, AuthProfileError::HomeMissing { .. }));
    }

    #[test]
    fn test_008_debug_redacts_env_values() {
        let mut env = BTreeMap::new();
        env.insert("CODEX_HOME".into(), "/secret/home".into());
        env.insert("XAI_API_KEY".into(), "sk-secret".into());
        let overlay = ProviderAuthOverlay {
            profile_id: "work".into(),
            provider: ProviderKind::Codex,
            env,
            unset: BTreeSet::new(),
            home: None,
        };
        let debug = format!("{overlay:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("sk-secret"));
        assert!(!debug.contains("/secret/home"));
        assert!(debug.contains("CODEX_HOME"));
    }

    #[test]
    fn empty_env_value_unsets_key() {
        let (_dir, home) = temp_home();
        let mut extra = BTreeMap::new();
        extra.insert("XAI_API_KEY".into(), String::new());
        extra.insert("GROK_EXTRA".into(), "1".into());
        let overlay = resolve(
            ProviderKind::Grok,
            Some("grok-alt"),
            None,
            &catalog_with(
                "grok-alt",
                ProviderAuthProfileDef {
                    provider: "grok".into(),
                    home: Some(home.display().to_string()),
                    env: extra,
                },
            ),
        )
        .unwrap();
        assert!(overlay.unset.contains("XAI_API_KEY"));
        assert_eq!(overlay.env.get("GROK_EXTRA"), Some(&"1".to_string()));
        assert!(overlay.env.contains_key("GROK_HOME"));
    }

    #[test]
    fn catalog_rejects_reserved_and_missing_required_home() {
        let mut catalog = HashMap::new();
        catalog.insert(
            "default".into(),
            ProviderAuthProfileDef {
                provider: "codex".into(),
                home: None,
                env: BTreeMap::new(),
            },
        );
        assert!(matches!(
            validate_catalog(&catalog),
            Err(AuthProfileError::ReservedProfileId)
        ));

        let mut catalog = HashMap::new();
        catalog.insert(
            "work".into(),
            ProviderAuthProfileDef {
                provider: "codex".into(),
                home: None,
                env: BTreeMap::new(),
            },
        );
        assert!(matches!(
            validate_catalog(&catalog),
            Err(AuthProfileError::HomeRequired { .. })
        ));
    }

    #[test]
    fn agy_named_profile_does_not_require_home() {
        let catalog = catalog_with(
            "agy-extra",
            ProviderAuthProfileDef {
                provider: "agy".into(),
                home: None,
                env: {
                    let mut env = BTreeMap::new();
                    env.insert("NVIDIA_API_KEY".into(), "from-secret".into());
                    env
                },
            },
        );
        validate_catalog(&catalog).unwrap();
        let overlay = resolve(
            intern_provider("agy").unwrap(),
            Some("agy-extra"),
            None,
            &catalog,
        )
        .unwrap();
        assert_eq!(overlay.provider.as_str(), "antigravity");
        assert_eq!(
            overlay.env.get("NVIDIA_API_KEY"),
            Some(&"from-secret".to_string())
        );
        assert!(overlay.home.is_none());
    }

    #[test]
    fn apply_overlay_sets_and_unsets() {
        let mut overlay = ProviderAuthOverlay::default_for(ProviderKind::Codex);
        overlay.env.insert("CODEX_HOME".into(), "/tmp/x".into());
        overlay.unset.insert("OPENAI_API_KEY".into());
        let mut command = Command::new("true");
        apply_overlay_to_command(&mut command, &overlay);
        let debug = format!("{overlay:?}");
        assert!(debug.contains("CODEX_HOME"));
        assert!(!debug.contains("/tmp/x"));
        let _ = fs::metadata("/tmp");
    }

    #[test]
    fn test_007_named_claude_profiles_flag_cswap_conflict() {
        assert!(named_claude_profile_ids(&HashMap::new()).is_empty());
        let (_dir, home) = temp_home();
        let catalog = catalog_with(
            "claude-b",
            ProviderAuthProfileDef {
                provider: "claude".into(),
                home: Some(home.display().to_string()),
                env: BTreeMap::new(),
            },
        );
        assert_eq!(
            named_claude_profile_ids(&catalog),
            vec!["claude-b".to_string()]
        );
    }

    #[test]
    fn test_012_empty_profile_home_never_copies_global() {
        let root = tempfile::tempdir().expect("profiles root");
        let global = tempfile::tempdir().expect("global home");
        fs::write(
            global.path().join("auth.json"),
            r#"{"tokens":{"access_token":"secret"}}"#,
        )
        .unwrap();
        let home = create_empty_profile_home_at(root.path(), &ProviderKind::Codex, "work").unwrap();
        assert!(home.is_dir());
        assert_ne!(home, global.path());
        assert!(!home.join("auth.json").exists());
        let listing: Vec<_> = fs::read_dir(&home)
            .unwrap()
            .filter_map(|entry| entry.ok().map(|value| value.file_name()))
            .collect();
        assert!(
            listing.is_empty(),
            "extra home must start empty, got {listing:?}"
        );
        let script = login_script_contents(
            &overlay_for_home(ProviderKind::Codex, "work", &home),
            vendor_login_argv(&ProviderKind::Codex).unwrap(),
        );
        assert!(script.contains("CODEX_HOME="));
        assert!(script.contains("codex"));
        assert!(script.contains("login"));
        assert!(script.contains("--device-auth"));
        assert!(
            !script
                .lines()
                .any(|line| line.starts_with("export HOME=") || line.starts_with("HOME=")),
            "extra-account login must not rewrite HOME"
        );
        assert!(!script.contains("secret"));
        let claude = login_script_contents(
            &overlay_for_home(ProviderKind::Claude, "claude-b", &home),
            vendor_login_argv(&ProviderKind::Claude).unwrap(),
        );
        assert!(claude.contains("CLAUDE_CONFIG_DIR="));
        assert!(claude.contains("auth"));
        assert!(claude.contains("--claudeai"));
        let grok = login_script_contents(
            &overlay_for_home(ProviderKind::Grok, "grok-alt", &home),
            vendor_login_argv(&ProviderKind::Grok).unwrap(),
        );
        assert!(grok.contains("--device-auth"));
        assert!(grok.contains("GROK_HOME="));
    }

    #[test]
    fn opencode_empty_home_creates_xdg_children_only() {
        let root = tempfile::tempdir().expect("profiles root");
        let home =
            create_empty_profile_home_at(root.path(), &ProviderKind::OpenCode, "oc-alt").unwrap();
        assert!(home.join("xdg-config").is_dir());
        assert!(home.join("xdg-data").is_dir());
        assert!(!home.join("xdg-data/opencode/auth.json").exists());
    }
}
