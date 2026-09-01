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
    ProfileHomeAlreadyExists {
        profile_id: String,
        home: String,
    },
    ProfileHomeSymlinkForbidden {
        profile_id: String,
        home: String,
    },
    ProfileParentSymlinkForbidden {
        profile_id: String,
        home: String,
    },
    UnsupportedLogin(String),
    GlobalHomeForbidden {
        provider: String,
        home: String,
    },
    UnmanagedHomeForbidden {
        profile_id: String,
        provider: String,
        home: String,
    },
    UnsupportedEnvironmentKey {
        profile_id: String,
        provider: String,
        key: String,
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
            Self::ProfileHomeAlreadyExists { profile_id, home } => write!(
                f,
                "auth_profile '{profile_id}' home '{home}' already exists; use reconnect or unlink explicitly"
            ),
            Self::ProfileHomeSymlinkForbidden { profile_id, home } => write!(
                f,
                "auth_profile '{profile_id}' home '{home}' must not be a symlink"
            ),
            Self::ProfileParentSymlinkForbidden { profile_id, home } => write!(
                f,
                "auth_profile '{profile_id}' managed parent for '{home}' must not be a symlink"
            ),
            Self::UnsupportedLogin(provider) => {
                write!(f, "extra-account login is not supported for '{provider}'")
            }
            Self::GlobalHomeForbidden { provider, home } => write!(
                f,
                "refusing to use global {provider} home '{home}' as an extra account"
            ),
            Self::UnmanagedHomeForbidden {
                profile_id,
                provider,
                home,
            } => write!(
                f,
                "auth_profile '{profile_id}' for {provider} must use its managed extra-account home, not '{home}'"
            ),
            Self::UnsupportedEnvironmentKey {
                profile_id,
                provider,
                key,
            } => write!(
                f,
                "auth_profile '{profile_id}' cannot set environment key '{key}' for {provider}"
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

/// The lifecycle intent determines whether the deterministic managed home must
/// already exist, must be absent, or is only being validated as catalog data.
/// Keeping this decision here prevents route/config callers from composing
/// incomplete path and symlink checks themselves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagedHomeValidationMode {
    Catalog,
    Existing,
    New,
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
        validate_profile_def(id, def)?;
    }
    Ok(())
}

pub fn validate_profile_def(
    id: &str,
    def: &ProviderAuthProfileDef,
) -> Result<(), AuthProfileError> {
    validate_profile_id(id)?;
    let provider = intern_provider(&def.provider)?;
    if named_profile_requires_home(&provider) {
        let home = def.home.as_deref().map(str::trim).unwrap_or("");
        if home.is_empty() {
            return Err(AuthProfileError::HomeRequired {
                profile_id: id.to_string(),
                provider: provider.as_str().to_string(),
            });
        }
        let home = PathBuf::from(expand_tilde(home));
        validate_managed_profile_home(&provider, id, &home, ManagedHomeValidationMode::Catalog)?;
    }
    for key in def.env.keys() {
        if !profile_env_key_allowed(&provider, key) {
            return Err(AuthProfileError::UnsupportedEnvironmentKey {
                profile_id: id.to_string(),
                provider: provider.as_str().to_string(),
                key: key.clone(),
            });
        }
    }
    Ok(())
}

/// Named accounts own only provider credentials.  They never control the
/// process home, executable search path, shell startup, or dynamic loader.
/// The resolver owns each provider's home variables exclusively.
pub fn profile_env_key_allowed(provider: &ProviderKind, key: &str) -> bool {
    provider_credential_env_keys(provider)
        .iter()
        .any(|candidate| *candidate == key)
}

/// Credentials inherited from the AgentDesk service environment must never
/// silently win over a named account's isolated home. Named profiles start by
/// clearing every credential key their provider can consume, then explicitly
/// opt back in only to credentials stored in that profile definition.
pub fn provider_credential_env_keys(provider: &ProviderKind) -> &'static [&'static str] {
    match provider {
        ProviderKind::Claude => &["ANTHROPIC_API_KEY"],
        ProviderKind::Codex => &["OPENAI_API_KEY"],
        ProviderKind::Grok => &["XAI_API_KEY"],
        ProviderKind::Qwen => &["DASHSCOPE_API_KEY", "OPENAI_API_KEY"],
        ProviderKind::OpenCode => &["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "XAI_API_KEY"],
        ProviderKind::Antigravity => &["NVIDIA_API_KEY"],
        ProviderKind::Gemini => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        ProviderKind::Unsupported(_) => &[],
    }
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
    resolve_at(
        &extra_accounts_root(),
        provider,
        channel_auth_profile,
        agent_auth_profile,
        catalog,
    )
}

fn resolve_at(
    managed_root: &Path,
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
        validate_managed_profile_home_at(
            managed_root,
            &provider,
            profile_id,
            home_path,
            ManagedHomeValidationMode::Existing,
        )?;
    }

    let mut env = home
        .as_ref()
        .map(|path| home_env_for(&provider, path))
        .unwrap_or_default();
    let mut unset: BTreeSet<String> = provider_credential_env_keys(&provider)
        .iter()
        .map(|key| (*key).to_string())
        .collect();
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

/// Returns true only for the deterministic, AgentDesk-managed home assigned to
/// this `(provider, profile_id)`.  This intentionally excludes imported or
/// global homes: otherwise a config/API caller could turn a named account into
/// an arbitrary credential-directory reader.
pub fn is_managed_extra_account_home(
    provider: &ProviderKind,
    profile_id: &str,
    home: &Path,
) -> bool {
    validate_managed_profile_home(
        provider,
        profile_id,
        home,
        ManagedHomeValidationMode::Catalog,
    )
    .is_ok()
}

pub fn extra_account_home_at(root: &Path, provider: &ProviderKind, profile_id: &str) -> PathBuf {
    root.join(provider.as_str()).join(profile_id)
}

pub fn validate_managed_profile_home(
    provider: &ProviderKind,
    profile_id: &str,
    home: &Path,
    mode: ManagedHomeValidationMode,
) -> Result<PathBuf, AuthProfileError> {
    validate_managed_profile_home_at(&extra_accounts_root(), provider, profile_id, home, mode)
}

pub fn validate_managed_profile_home_at(
    root: &Path,
    provider: &ProviderKind,
    profile_id: &str,
    home: &Path,
    mode: ManagedHomeValidationMode,
) -> Result<PathBuf, AuthProfileError> {
    validate_profile_id(profile_id)?;
    let expected = extra_account_home_at(root, provider, profile_id);
    if home != expected {
        return Err(AuthProfileError::UnmanagedHomeForbidden {
            profile_id: profile_id.to_string(),
            provider: provider.as_str().to_string(),
            home: home.display().to_string(),
        });
    }

    // The managed root and provider directory form the trust boundary.  A
    // leaf-only lstat is insufficient because a symlinked parent can redirect
    // a seemingly ordinary profile directory outside AgentDesk ownership.
    let provider_root = root.join(provider.as_str());
    for parent in [root, provider_root.as_path()] {
        if std::fs::symlink_metadata(parent)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(AuthProfileError::ProfileParentSymlinkForbidden {
                profile_id: profile_id.to_string(),
                home: home.display().to_string(),
            });
        }
    }
    match std::fs::symlink_metadata(home) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(AuthProfileError::ProfileHomeSymlinkForbidden {
                profile_id: profile_id.to_string(),
                home: home.display().to_string(),
            });
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(AuthProfileError::Io(format!(
                "profile home '{}' exists but is not a directory",
                home.display()
            )));
        }
        Ok(_) if mode == ManagedHomeValidationMode::New => {
            return Err(AuthProfileError::ProfileHomeAlreadyExists {
                profile_id: profile_id.to_string(),
                home: home.display().to_string(),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if mode == ManagedHomeValidationMode::Existing {
                return Err(AuthProfileError::HomeMissing {
                    profile_id: profile_id.to_string(),
                    home: home.display().to_string(),
                });
            }
        }
        Err(error) => {
            return Err(AuthProfileError::Io(format!(
                "inspect profile home '{}': {error}",
                home.display()
            )));
        }
        _ => {}
    }
    Ok(expected)
}

/// Includes homes left behind by unlink.  They remain credential-bearing
/// paths and must reserve their profile id until an explicit reconnect/delete
/// lifecycle is introduced.
pub fn list_existing_profile_homes(
    provider: &ProviderKind,
) -> Result<Vec<PathBuf>, AuthProfileError> {
    let provider_root = extra_accounts_root().join(provider.as_str());
    match std::fs::read_dir(&provider_root) {
        Ok(entries) => Ok(entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(AuthProfileError::Io(format!(
            "read provider profile root '{}': {error}",
            provider_root.display()
        ))),
    }
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
        if home.exists()
            || existing_homes.iter().any(|path| {
                path == &home
                    || path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name == candidate)
            })
        {
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
    validate_managed_profile_home_at(
        root,
        provider,
        profile_id,
        &home,
        ManagedHomeValidationMode::New,
    )?;
    let provider_root = root.join(provider.as_str());
    std::fs::create_dir_all(&provider_root).map_err(|error| {
        AuthProfileError::Io(format!(
            "create provider profile root '{}': {error}",
            provider_root.display()
        ))
    })?;
    // create_dir, rather than create_dir_all(home), makes a concurrent or
    // previously unlinked home a conflict instead of silently reusing its
    // credentials.
    std::fs::create_dir(&home).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            AuthProfileError::ProfileHomeAlreadyExists {
                profile_id: profile_id.to_string(),
                home: home.display().to_string(),
            }
        } else {
            AuthProfileError::Io(format!("create profile home '{}': {error}", home.display()))
        }
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

/// Render an auth overlay as shell statements for provider-owned launch
/// scripts.  Unsets are intentionally emitted before exports so a named
/// profile cannot inherit a service-level API key while still being able to
/// supply an explicitly configured key of its own.
pub fn overlay_shell_env_lines(overlay: &ProviderAuthOverlay) -> String {
    let mut body = String::new();
    for key in &overlay.unset {
        body.push_str("unset ");
        body.push_str(key);
        body.push('\n');
    }
    for (key, value) in &overlay.env {
        body.push_str("export ");
        body.push_str(key);
        body.push('=');
        body.push_str(&crate::services::process::shell_escape(value));
        body.push('\n');
    }
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
        let (dir, _) = temp_home();
        let cases = [
            (ProviderKind::Codex, "work", "CODEX_HOME"),
            (ProviderKind::Qwen, "qwen-b", "QWEN_HOME"),
            (ProviderKind::Claude, "claude-b", "CLAUDE_CONFIG_DIR"),
            (ProviderKind::OpenCode, "oc-alt", "XDG_CONFIG_HOME"),
            (ProviderKind::Grok, "grok-alt", "GROK_HOME"),
        ];
        for (provider, id, home_key) in cases {
            let home = create_empty_profile_home_at(dir.path(), &provider, id).unwrap();
            let overlay = resolve_at(
                dir.path(),
                provider.clone(),
                Some(id),
                None,
                &catalog_with(
                    id,
                    ProviderAuthProfileDef {
                        provider: provider.as_str().to_string(),
                        home: Some(home.display().to_string()),
                        env: BTreeMap::new(),
                    },
                ),
            )
            .unwrap();
            assert!(!overlay.env.contains_key("HOME"));
            let expected_home_value = if provider == ProviderKind::OpenCode {
                format!("{}/xdg-config", home.display())
            } else {
                home.display().to_string()
            };
            assert_eq!(overlay.env.get(home_key), Some(&expected_home_value));
            if provider == ProviderKind::OpenCode {
                assert_eq!(
                    overlay.env.get("XDG_DATA_HOME"),
                    Some(&format!("{}/xdg-data", home.display()))
                );
            }
        }
    }

    #[test]
    fn named_profile_clears_inherited_provider_credentials() {
        let (dir, _) = temp_home();
        let home = create_empty_profile_home_at(dir.path(), &ProviderKind::Codex, "work")
            .expect("profile home");
        let overlay = resolve_at(
            dir.path(),
            ProviderKind::Codex,
            Some("work"),
            None,
            &catalog_with(
                "work",
                ProviderAuthProfileDef {
                    provider: "codex".to_string(),
                    home: Some(home.display().to_string()),
                    env: BTreeMap::new(),
                },
            ),
        )
        .expect("named profile");

        assert!(overlay.unset.contains("OPENAI_API_KEY"));
        let merged = merge_overlay_env(
            vec![("OPENAI_API_KEY".to_string(), "parent-secret".to_string())],
            &overlay,
        );
        assert!(merged.iter().all(|(key, _)| key != "OPENAI_API_KEY"));
        assert_eq!(
            merged
                .iter()
                .find(|(key, _)| key == "CODEX_HOME")
                .map(|(_, value)| value),
            Some(&home.display().to_string())
        );
    }

    #[test]
    fn test_003_channel_overrides_agent() {
        let (dir, _) = temp_home();
        let work = create_empty_profile_home_at(dir.path(), &ProviderKind::Codex, "work").unwrap();
        let other =
            create_empty_profile_home_at(dir.path(), &ProviderKind::Codex, "other").unwrap();
        let mut catalog = HashMap::new();
        catalog.insert(
            "work".to_string(),
            ProviderAuthProfileDef {
                provider: "codex".into(),
                home: Some(work.display().to_string()),
                env: BTreeMap::new(),
            },
        );
        catalog.insert(
            "other".to_string(),
            ProviderAuthProfileDef {
                provider: "codex".into(),
                home: Some(other.display().to_string()),
                env: BTreeMap::new(),
            },
        );
        let overlay = resolve_at(
            dir.path(),
            ProviderKind::Codex,
            Some("work"),
            Some("other"),
            &catalog,
        )
        .unwrap();
        assert_eq!(overlay.profile_id, "work");
    }

    #[test]
    fn test_004_unknown_mismatch_missing_home_fail_closed() {
        let err = resolve(ProviderKind::Codex, Some("missing"), None, &HashMap::new()).unwrap_err();
        assert!(matches!(err, AuthProfileError::UnknownProfile(_)));

        let (dir, _) = temp_home();
        let qwen_home =
            create_empty_profile_home_at(dir.path(), &ProviderKind::Qwen, "work").unwrap();
        let catalog = catalog_with(
            "work",
            ProviderAuthProfileDef {
                provider: "qwen".into(),
                home: Some(qwen_home.display().to_string()),
                env: BTreeMap::new(),
            },
        );
        let err = resolve_at(
            dir.path(),
            ProviderKind::Codex,
            Some("work"),
            None,
            &catalog,
        )
        .unwrap_err();
        assert!(matches!(err, AuthProfileError::ProviderMismatch { .. }));

        let catalog = catalog_with(
            "work",
            ProviderAuthProfileDef {
                provider: "codex".into(),
                home: Some(
                    extra_account_home_at(dir.path(), &ProviderKind::Codex, "work")
                        .display()
                        .to_string(),
                ),
                env: BTreeMap::new(),
            },
        );
        let err = resolve_at(
            dir.path(),
            ProviderKind::Codex,
            Some("work"),
            None,
            &catalog,
        )
        .unwrap_err();
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
        let (dir, _) = temp_home();
        let home =
            create_empty_profile_home_at(dir.path(), &ProviderKind::Grok, "grok-alt").unwrap();
        let mut extra = BTreeMap::new();
        extra.insert("XAI_API_KEY".into(), String::new());
        extra.insert("GROK_EXTRA".into(), "1".into());
        let overlay = resolve_at(
            dir.path(),
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
    fn catalog_accepts_only_its_managed_home_and_credential_env_keys() {
        let profile_id = "work";
        let managed_home = extra_account_home(&ProviderKind::Codex, profile_id);
        let catalog = catalog_with(
            profile_id,
            ProviderAuthProfileDef {
                provider: "codex".into(),
                home: Some(managed_home.display().to_string()),
                env: BTreeMap::from([("OPENAI_API_KEY".into(), "profile-key".into())]),
            },
        );
        validate_catalog(&catalog).unwrap();

        let outside_catalog = catalog_with(
            profile_id,
            ProviderAuthProfileDef {
                provider: "codex".into(),
                home: Some("/tmp/not-an-agentdesk-profile".into()),
                env: BTreeMap::new(),
            },
        );
        assert!(matches!(
            validate_catalog(&outside_catalog),
            Err(AuthProfileError::UnmanagedHomeForbidden { .. })
        ));

        let home_override_catalog = catalog_with(
            profile_id,
            ProviderAuthProfileDef {
                provider: "codex".into(),
                home: Some(managed_home.display().to_string()),
                env: BTreeMap::from([("HOME".into(), "/tmp/unsafe".into())]),
            },
        );
        assert!(matches!(
            validate_catalog(&home_override_catalog),
            Err(AuthProfileError::UnsupportedEnvironmentKey { .. })
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
    fn named_profile_shell_overlay_clears_parent_key_before_exporting_home() {
        let mut overlay = ProviderAuthOverlay::default_for(ProviderKind::Codex);
        overlay.profile_id = "codex-work".to_string();
        overlay.unset.insert("OPENAI_API_KEY".to_string());
        overlay
            .env
            .insert("CODEX_HOME".to_string(), "/tmp/work home".to_string());

        let lines = overlay_shell_env_lines(&overlay);
        assert!(lines.starts_with("unset OPENAI_API_KEY\n"));
        assert!(lines.contains("export CODEX_HOME='/tmp/work home'\n"));
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

    #[test]
    fn new_profile_rejects_existing_unlinked_home_and_allocation_skips_it() {
        let root = tempfile::tempdir().expect("profiles root");
        let old_home = create_empty_profile_home_at(root.path(), &ProviderKind::Codex, "codex-alt")
            .expect("first profile home");
        fs::write(old_home.join("auth.json"), "old credential").expect("fake credential");

        let err = create_empty_profile_home_at(root.path(), &ProviderKind::Codex, "codex-alt")
            .expect_err("an existing home is never a new account");
        assert!(matches!(
            err,
            AuthProfileError::ProfileHomeAlreadyExists { .. }
        ));
        assert_eq!(
            fs::read_to_string(old_home.join("auth.json")).unwrap(),
            "old credential"
        );

        let catalog = HashMap::new(); // models the post-unlink catalog state
        let allocated = allocate_profile_id(&ProviderKind::Codex, &catalog, &[old_home]).unwrap();
        assert_eq!(allocated, "codex-alt-2");
    }

    #[test]
    fn existing_mode_requires_a_real_managed_directory() {
        let root = tempfile::tempdir().expect("profiles root");
        let home = extra_account_home_at(root.path(), &ProviderKind::Codex, "work");
        let err = validate_managed_profile_home_at(
            root.path(),
            &ProviderKind::Codex,
            "work",
            &home,
            ManagedHomeValidationMode::Existing,
        )
        .expect_err("completion/resolve needs an existing directory");
        assert!(matches!(err, AuthProfileError::HomeMissing { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn managed_parent_symlink_is_rejected_before_profile_creation() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("profiles root");
        let outside = tempfile::tempdir().expect("outside root");
        symlink(outside.path(), root.path().join("codex")).expect("provider parent symlink");
        let home = extra_account_home_at(root.path(), &ProviderKind::Codex, "work");
        let err = validate_managed_profile_home_at(
            root.path(),
            &ProviderKind::Codex,
            "work",
            &home,
            ManagedHomeValidationMode::New,
        )
        .expect_err("a symlinked provider parent escapes the managed root");
        assert!(matches!(
            err,
            AuthProfileError::ProfileParentSymlinkForbidden { .. }
        ));
    }
}
