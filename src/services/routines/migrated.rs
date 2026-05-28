use serde_json::Value;
use std::collections::HashMap;

use crate::error::{AppError, AppResult};
use crate::services::operator_connectors::{
    OptionalConnectorState, optional_connector_status_by_id,
};

pub fn is_migrated_launchd_script_ref(script_ref: &str) -> bool {
    script_ref.starts_with("migrated-launchd/")
}

pub fn validate_migrated_launchd_activation(
    script_ref: &str,
    checkpoint: Option<&Value>,
    metadata: Option<&Value>,
    routine_dirs: &[std::path::PathBuf],
) -> AppResult<()> {
    if !is_migrated_launchd_script_ref(script_ref) {
        return Ok(());
    }
    validate_migrated_launchd_entrypoint(script_ref, metadata, routine_dirs)?;
    validate_migrated_launchd_required_paths(script_ref, metadata, checkpoint)?;
    validate_migrated_launchd_required_connectors(script_ref, metadata, checkpoint)
}

fn validate_migrated_launchd_entrypoint(
    script_ref: &str,
    metadata: Option<&Value>,
    routine_dirs: &[std::path::PathBuf],
) -> AppResult<()> {
    let relative = migrated_launchd_entrypoint_relative_path(script_ref, metadata)?;
    let mut candidates = Vec::new();
    if let Some(root) = crate::config::runtime_root() {
        push_entrypoint_candidate(&mut candidates, root.join(&relative));
    }
    if let Ok(cwd) = std::env::current_dir() {
        push_entrypoint_candidate(&mut candidates, cwd.join(&relative));
    }
    for dir in routine_dirs {
        push_entrypoint_candidate(&mut candidates, dir.join(&relative));
        if let Some(parent) = dir.parent() {
            push_entrypoint_candidate(&mut candidates, parent.join(&relative));
        }
    }
    if candidates.iter().any(|candidate| candidate.is_file()) {
        return Ok(());
    }
    let checked = candidates
        .iter()
        .map(|candidate| candidate.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(AppError::conflict(format!(
        "migrated routine {script_ref} is invalid: shell entrypoint not found ({checked})"
    )))
}

fn push_entrypoint_candidate(
    candidates: &mut Vec<std::path::PathBuf>,
    candidate: std::path::PathBuf,
) {
    if !candidates.iter().any(|existing| existing == &candidate) {
        candidates.push(candidate);
    }
}

fn migrated_launchd_entrypoint_relative_path(
    script_ref: &str,
    metadata: Option<&Value>,
) -> AppResult<std::path::PathBuf> {
    for pointer in ["/migrated_launchd/entrypoint", "/portable/entrypoint"] {
        let Some(value) = metadata.and_then(|metadata| metadata.pointer(pointer)) else {
            continue;
        };
        let raw = value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                AppError::conflict(format!(
                    "migrated routine metadata field {pointer} must be a non-empty string"
                ))
            })?;
        return validate_relative_entrypoint_path(raw);
    }

    let stem = std::path::Path::new(script_ref)
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            AppError::conflict(format!(
                "migrated routine {script_ref} is invalid: script_ref has no file stem"
            ))
        })?;
    Ok(std::path::PathBuf::from("scripts")
        .join("launchd-migrated")
        .join(format!("{stem}.sh")))
}

fn validate_relative_entrypoint_path(raw_path: &str) -> AppResult<std::path::PathBuf> {
    let normalized = raw_path.trim().replace('\\', "/");
    let path = std::path::PathBuf::from(&normalized);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::Prefix(_)
                    | std::path::Component::RootDir
            )
        })
    {
        return Err(AppError::conflict(
            "migrated routine metadata entrypoint must be a repo-relative path",
        ));
    }
    Ok(path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequiredPathKind {
    Any,
    File,
    Dir,
}

impl RequiredPathKind {
    fn from_value(value: Option<&Value>) -> AppResult<Self> {
        let Some(value) = value else {
            return Ok(Self::Any);
        };
        let Some(kind) = value
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Err(AppError::conflict(
                "migrated routine required path kind must be a non-empty string",
            ));
        };
        match kind {
            "any" | "path" => Ok(Self::Any),
            "file" => Ok(Self::File),
            "dir" | "directory" => Ok(Self::Dir),
            other => Err(AppError::conflict(format!(
                "unsupported migrated routine required path kind '{other}'"
            ))),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Any => "path",
            Self::File => "file",
            Self::Dir => "directory",
        }
    }

    fn exists(self, path: &std::path::Path) -> bool {
        match self {
            Self::Any => path.exists(),
            Self::File => path.is_file(),
            Self::Dir => path.is_dir(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequiredPath {
    label: String,
    path: std::path::PathBuf,
    kind: RequiredPathKind,
}

fn validate_migrated_launchd_required_paths(
    script_ref: &str,
    metadata: Option<&Value>,
    checkpoint: Option<&Value>,
) -> AppResult<()> {
    let mut required_paths = migrated_launchd_builtin_required_paths(script_ref)?;
    required_paths.extend(migrated_required_paths_from_metadata(metadata)?);
    required_paths.extend(migrated_required_paths_from_checkpoint(checkpoint)?);
    for required in required_paths {
        if required.kind.exists(&required.path) {
            continue;
        }
        return Err(AppError::conflict(format!(
            "migrated routine {script_ref} is invalid: required {} '{}' not found: {}",
            required.kind.label(),
            required.label,
            required.path.display()
        )));
    }
    Ok(())
}

fn migrated_launchd_builtin_required_paths(script_ref: &str) -> AppResult<Vec<RequiredPath>> {
    let mut paths = Vec::new();
    if matches!(
        script_ref,
        "migrated-launchd/memento-daily-report.js"
            | "migrated-launchd/memento-hygiene.js"
            | "migrated-launchd/memory-merge.js"
    ) {
        let workdir = migrated_env_path("AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR")
            .or_else(|| migrated_workspace_path("agentfactory"))
            .ok_or_else(|| {
                AppError::conflict(format!(
                    "migrated routine {script_ref} is invalid: AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR is unavailable"
                ))
            })?;
        paths.push(RequiredPath {
            label: "AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR".to_string(),
            path: workdir,
            kind: RequiredPathKind::Dir,
        });
    }
    if script_ref == "migrated-launchd/memory-merge.js" {
        let skill = migrated_env_path("AGENTDESK_MEMORY_MERGE_SKILL")
            .or_else(|| {
                crate::config::runtime_root()
                    .map(|root| root.join("skills").join("memory-merge").join("SKILL.md"))
            })
            .ok_or_else(|| {
                AppError::conflict(
                    "migrated routine memory-merge is invalid: AGENTDESK_MEMORY_MERGE_SKILL is unavailable",
                )
            })?;
        paths.push(RequiredPath {
            label: "AGENTDESK_MEMORY_MERGE_SKILL".to_string(),
            path: skill,
            kind: RequiredPathKind::File,
        });
    }
    match script_ref {
        "migrated-launchd/agent-feedback-briefing.js" => {
            push_migrated_env_required_path(
                script_ref,
                &mut paths,
                "AGENTDESK_AGENT_FEEDBACK_INBOX",
                "agent feedback inbox",
                RequiredPathKind::Dir,
            )?;
        }
        "migrated-launchd/ai-integrated-briefing.js" => {
            push_migrated_skill_required_path(
                script_ref,
                &mut paths,
                "ai-integrated-briefing/SKILL.md",
                "ai-integrated-briefing skill",
                RequiredPathKind::File,
            )?;
        }
        "migrated-launchd/banchan-day-reminder-cook.js"
        | "migrated-launchd/banchan-day-reminder-prep.js" => {
            push_migrated_skill_required_path(
                script_ref,
                &mut paths,
                "banchan-day-reminder/SKILL.md",
                "banchan-day-reminder skill",
                RequiredPathKind::File,
            )?;
            push_migrated_skill_required_path(
                script_ref,
                &mut paths,
                "banchan-day-reminder/references/messages.md",
                "banchan-day-reminder messages",
                RequiredPathKind::File,
            )?;
        }
        "migrated-launchd/cookingheart-daily-briefing.js" => {
            push_migrated_skill_required_path(
                script_ref,
                &mut paths,
                "cookingheart-daily-briefing/SKILL.md",
                "cookingheart-daily-briefing skill",
                RequiredPathKind::File,
            )?;
        }
        "migrated-launchd/family-morning-briefing-obujang.js"
        | "migrated-launchd/family-morning-briefing-yohoejang.js" => {
            push_migrated_skill_required_path(
                script_ref,
                &mut paths,
                "family-morning-briefing/scripts",
                "family-morning-briefing scripts",
                RequiredPathKind::Dir,
            )?;
        }
        _ => {}
    }
    Ok(paths)
}

fn push_migrated_env_required_path(
    script_ref: &str,
    paths: &mut Vec<RequiredPath>,
    env_name: &str,
    label: &str,
    kind: RequiredPathKind,
) -> AppResult<()> {
    let path = migrated_env_path(env_name).ok_or_else(|| {
        AppError::conflict(format!(
            "migrated routine {script_ref} is invalid: {env_name} is unavailable"
        ))
    })?;
    paths.push(RequiredPath {
        label: label.to_string(),
        path,
        kind,
    });
    Ok(())
}

fn push_migrated_skill_required_path(
    script_ref: &str,
    paths: &mut Vec<RequiredPath>,
    relative_path: &str,
    label: &str,
    kind: RequiredPathKind,
) -> AppResult<()> {
    let skill_root = migrated_env_path("AGENTDESK_OBSIDIAN_SKILL_ROOT").ok_or_else(|| {
        AppError::conflict(format!(
            "migrated routine {script_ref} is invalid: AGENTDESK_OBSIDIAN_SKILL_ROOT is unavailable"
        ))
    })?;
    paths.push(RequiredPath {
        label: label.to_string(),
        path: skill_root.join(relative_path),
        kind,
    });
    Ok(())
}

fn migrated_workspace_path(name: &str) -> Option<std::path::PathBuf> {
    migrated_env_path("AGENTDESK_WORKSPACE_ROOT")
        .map(|root| root.join(name))
        .or_else(|| crate::config::runtime_root().map(|root| root.join("workspaces").join(name)))
}

fn migrated_required_paths_from_checkpoint(
    checkpoint: Option<&Value>,
) -> AppResult<Vec<RequiredPath>> {
    migrated_required_paths_from_value("checkpoint", checkpoint)
}

fn migrated_required_paths_from_metadata(metadata: Option<&Value>) -> AppResult<Vec<RequiredPath>> {
    migrated_required_paths_from_value("metadata", metadata)
}

fn migrated_required_paths_from_value(
    source: &str,
    value: Option<&Value>,
) -> AppResult<Vec<RequiredPath>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let mut paths = Vec::new();
    for pointer in [
        "/migrated_launchd/required_paths",
        "/portable/required_paths",
    ] {
        let Some(value) = value.pointer(pointer) else {
            continue;
        };
        let Some(entries) = value.as_array() else {
            return Err(AppError::conflict(format!(
                "migrated routine {source} field {pointer} must be an array"
            )));
        };
        for entry in entries {
            if let Some(required) = parse_migrated_required_path(entry)? {
                paths.push(required);
            }
        }
    }
    Ok(paths)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequiredConnector {
    id: String,
    label: String,
}

fn validate_migrated_launchd_required_connectors(
    script_ref: &str,
    metadata: Option<&Value>,
    checkpoint: Option<&Value>,
) -> AppResult<()> {
    let mut required_connectors = migrated_required_connectors_from_value("metadata", metadata)?;
    required_connectors.extend(migrated_required_connectors_from_value(
        "checkpoint",
        checkpoint,
    )?);
    for required in required_connectors {
        if migrated_required_connector_is_ready(&required.id) {
            continue;
        }
        let Some(status) = optional_connector_status_by_id(&required.id) else {
            return Err(AppError::conflict(format!(
                "migrated routine {script_ref} is invalid: unsupported required connector '{}'",
                required.id
            )));
        };
        if status.state == OptionalConnectorState::Ready {
            continue;
        }
        let setup = if status.setup_actions.is_empty() {
            String::new()
        } else {
            format!(" setup: {}", status.setup_actions.join(" | "))
        };
        return Err(AppError::conflict(format!(
            "migrated routine {script_ref} is invalid: required connector '{}' is {}: {}{}",
            required.label,
            status.state.as_str(),
            status.detail,
            setup
        )));
    }
    Ok(())
}

fn migrated_required_connector_is_ready(id: &str) -> bool {
    match id {
        "obsidian_skill_root" => migrated_env_path("AGENTDESK_OBSIDIAN_SKILL_ROOT")
            .as_deref()
            .is_some_and(migrated_obsidian_skill_root_contains_skill),
        "obsidian_agent_prompts" => migrated_env_path("AGENTDESK_OBSIDIAN_AGENTS_SRC")
            .as_deref()
            .is_some_and(std::path::Path::is_dir),
        _ => false,
    }
}

fn migrated_obsidian_skill_root_contains_skill(root: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let path = entry.path();
        path.is_dir() && path.join("SKILL.md").is_file()
    })
}

fn migrated_required_connectors_from_value(
    source: &str,
    value: Option<&Value>,
) -> AppResult<Vec<RequiredConnector>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let mut connectors = Vec::new();
    for pointer in [
        "/migrated_launchd/required_connectors",
        "/portable/required_connectors",
    ] {
        let Some(value) = value.pointer(pointer) else {
            continue;
        };
        let Some(entries) = value.as_array() else {
            return Err(AppError::conflict(format!(
                "migrated routine {source} field {pointer} must be an array"
            )));
        };
        for entry in entries {
            if let Some(required) = parse_migrated_required_connector(entry)? {
                connectors.push(required);
            }
        }
    }
    Ok(connectors)
}

fn parse_migrated_required_connector(value: &Value) -> AppResult<Option<RequiredConnector>> {
    if let Some(id) = value.as_str() {
        let id = id.trim();
        if id.is_empty() {
            return Err(AppError::conflict(
                "migrated routine required connector must not be empty",
            ));
        }
        return Ok(Some(RequiredConnector {
            id: id.to_string(),
            label: id.to_string(),
        }));
    }

    let Some(object) = value.as_object() else {
        return Err(AppError::conflict(
            "migrated routine required connector must be a string or object",
        ));
    };
    if object
        .get("required")
        .and_then(Value::as_bool)
        .is_some_and(|required| !required)
    {
        return Ok(None);
    }
    let id = object
        .get("id")
        .or_else(|| object.get("connector"))
        .or_else(|| object.get("capability"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AppError::conflict("migrated routine required connector object needs a non-empty id")
        })?;
    let label = object
        .get("label")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(id);
    Ok(Some(RequiredConnector {
        id: id.to_string(),
        label: label.to_string(),
    }))
}

fn parse_migrated_required_path(value: &Value) -> AppResult<Option<RequiredPath>> {
    if let Some(path) = value.as_str() {
        let raw_path = path.trim();
        if raw_path.is_empty() {
            return Err(AppError::conflict(
                "migrated routine required path must not be empty",
            ));
        }
        return Ok(Some(RequiredPath {
            label: raw_path.to_string(),
            path: expand_migrated_required_path(raw_path)?,
            kind: RequiredPathKind::Any,
        }));
    }

    let Some(object) = value.as_object() else {
        return Err(AppError::conflict(
            "migrated routine required path must be a string or object",
        ));
    };
    if object
        .get("required")
        .and_then(Value::as_bool)
        .is_some_and(|required| !required)
    {
        return Ok(None);
    }
    let raw_path = object
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| {
            AppError::conflict("migrated routine required path object needs a non-empty path")
        })?;
    let label = object
        .get("label")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .unwrap_or(raw_path);
    Ok(Some(RequiredPath {
        label: label.to_string(),
        path: expand_migrated_required_path(raw_path)?,
        kind: RequiredPathKind::from_value(object.get("kind"))?,
    }))
}

fn expand_migrated_required_path(raw_path: &str) -> AppResult<std::path::PathBuf> {
    let trimmed = raw_path.trim();
    if trimmed == "~" {
        return dirs::home_dir().ok_or_else(|| {
            AppError::conflict("migrated routine required path uses ~ but home is unavailable")
        });
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        let home = dirs::home_dir().ok_or_else(|| {
            AppError::conflict("migrated routine required path uses ~ but home is unavailable")
        })?;
        return Ok(home.join(rest));
    }
    if let Some(rest) = trimmed.strip_prefix("${")
        && let Some((name, suffix)) = rest.split_once('}')
    {
        return expand_migrated_env_required_path(name, suffix);
    }
    if let Some(rest) = trimmed.strip_prefix('$') {
        let name_len = rest
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
            .map(char::len_utf8)
            .sum::<usize>();
        if name_len > 0 {
            let (name, suffix) = rest.split_at(name_len);
            return expand_migrated_env_required_path(name, suffix);
        }
    }
    Ok(std::path::PathBuf::from(trimmed))
}

fn expand_migrated_env_required_path(
    env_name: &str,
    suffix: &str,
) -> AppResult<std::path::PathBuf> {
    let base = migrated_env_path(env_name).ok_or_else(|| {
        AppError::conflict(format!(
            "migrated routine required path references unset env var {env_name}"
        ))
    })?;
    let suffix = suffix.trim_start_matches(['/', '\\']);
    if suffix.is_empty() {
        Ok(base)
    } else {
        Ok(base.join(suffix))
    }
}

fn migrated_env_path(env_name: &str) -> Option<std::path::PathBuf> {
    if let Some(value) = std::env::var_os(env_name).filter(|value| !value.is_empty()) {
        return Some(std::path::PathBuf::from(value));
    }
    let profile_env = migrated_zprofile_env();
    migrated_env_string_inner(env_name, &profile_env, 0).map(std::path::PathBuf::from)
}

const MIGRATED_RESOLVER_ENV_KEYS: &[&str] = &[
    "HOME",
    "USERPROFILE",
    "AGENTDESK_ROOT_DIR",
    "AGENTDESK_WORKSPACE_ROOT",
    "AGENTDESK_MIGRATED_ENTRYPOINT_DIR",
    "AGENTDESK_OPERATOR_WORKDIR",
    "AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR",
    "OBSIDIAN_VAULT_ROOT",
    "OBSIDIAN_REMOTE_VAULT_ROOT",
    "AGENTDESK_OBSIDIAN_AGENTS_SRC",
    "AGENTDESK_OBSIDIAN_SKILL_ROOT",
    "AGENTDESK_AGENT_FEEDBACK_INBOX",
    "AGENTDESK_MEMORY_MERGE_SKILL",
];

fn migrated_zprofile_env() -> HashMap<String, String> {
    if std::env::var("AGENTDESK_SOURCE_ZPROFILE").ok().as_deref() == Some("0") {
        return HashMap::new();
    }
    let Some(home) = migrated_home_dir() else {
        return HashMap::new();
    };
    let Ok(contents) = std::fs::read_to_string(home.join(".zprofile")) else {
        return HashMap::new();
    };
    let mut env = HashMap::new();
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let assignment = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((key, value)) = assignment.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if !MIGRATED_RESOLVER_ENV_KEYS.contains(&key)
            || !key
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        {
            continue;
        }
        let value = strip_shell_quotes(value.trim());
        if !value.is_empty() {
            env.insert(key.to_string(), value);
        }
    }
    env
}

fn migrated_env_string_inner(
    env_name: &str,
    profile_env: &HashMap<String, String>,
    depth: usize,
) -> Option<String> {
    if depth > 16 {
        return None;
    }
    if let Ok(value) = std::env::var(env_name)
        && !value.is_empty()
    {
        return Some(value);
    }
    if let Some(value) = profile_env.get(env_name).filter(|value| !value.is_empty()) {
        return migrated_expand_env_value(value, profile_env, depth + 1);
    }
    let value = migrated_env_default_value(env_name, profile_env, depth + 1)?;
    migrated_expand_env_value(&value, profile_env, depth + 1)
}

fn migrated_env_default_value(
    env_name: &str,
    profile_env: &HashMap<String, String>,
    depth: usize,
) -> Option<String> {
    match env_name {
        "HOME" => migrated_home_dir().map(path_to_string),
        "USERPROFILE" => std::env::var_os("USERPROFILE")
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from)
            .map(path_to_string),
        "AGENTDESK_ROOT_DIR" => migrated_env_path_string("HOME", profile_env, depth).map(|home| {
            path_to_string(std::path::PathBuf::from(home).join(".adk").join("release"))
        }),
        "AGENTDESK_WORKSPACE_ROOT" => {
            migrated_env_path_string("AGENTDESK_ROOT_DIR", profile_env, depth)
                .map(|root| path_to_string(std::path::PathBuf::from(root).join("workspaces")))
        }
        "AGENTDESK_MIGRATED_ENTRYPOINT_DIR" => {
            migrated_env_path_string("AGENTDESK_ROOT_DIR", profile_env, depth).map(|root| {
                path_to_string(
                    std::path::PathBuf::from(root)
                        .join("scripts")
                        .join("launchd-migrated"),
                )
            })
        }
        "AGENTDESK_OPERATOR_WORKDIR" => migrated_env_path_string("HOME", profile_env, depth),
        "AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR" => {
            migrated_env_path_string("AGENTDESK_WORKSPACE_ROOT", profile_env, depth)
                .map(|root| path_to_string(std::path::PathBuf::from(root).join("agentfactory")))
        }
        "OBSIDIAN_VAULT_ROOT" => {
            let root = migrated_env_path_string("AGENTDESK_ROOT_DIR", profile_env, depth)
                .map(std::path::PathBuf::from);
            if let Some(runtime_vault) = root.map(|root| root.join("ObsidianVault"))
                && runtime_vault.is_dir()
            {
                return Some(path_to_string(runtime_vault));
            }
            migrated_env_path_string("HOME", profile_env, depth)
                .map(|home| path_to_string(std::path::PathBuf::from(home).join("ObsidianVault")))
        }
        "OBSIDIAN_REMOTE_VAULT_ROOT" => {
            migrated_env_path_string("OBSIDIAN_VAULT_ROOT", profile_env, depth)
                .map(|root| path_to_string(std::path::PathBuf::from(root).join("RemoteVault")))
        }
        "AGENTDESK_OBSIDIAN_AGENTS_SRC" => {
            migrated_env_path_string("OBSIDIAN_REMOTE_VAULT_ROOT", profile_env, depth).map(|root| {
                path_to_string(
                    std::path::PathBuf::from(root)
                        .join("adk-config")
                        .join("agents"),
                )
            })
        }
        "AGENTDESK_OBSIDIAN_SKILL_ROOT" => {
            migrated_env_path_string("OBSIDIAN_REMOTE_VAULT_ROOT", profile_env, depth)
                .map(|root| path_to_string(std::path::PathBuf::from(root).join("99_Skills")))
        }
        "AGENTDESK_AGENT_FEEDBACK_INBOX" => {
            migrated_env_path_string("OBSIDIAN_REMOTE_VAULT_ROOT", profile_env, depth).map(|root| {
                path_to_string(
                    std::path::PathBuf::from(root)
                        .join("agents")
                        .join("ch-pmd")
                        .join("inbox"),
                )
            })
        }
        _ => None,
    }
}

fn migrated_env_path_string(
    env_name: &str,
    profile_env: &HashMap<String, String>,
    depth: usize,
) -> Option<String> {
    migrated_env_string_inner(env_name, profile_env, depth)
}

fn migrated_home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|value| !value.is_empty())
                .map(std::path::PathBuf::from)
        })
        .or_else(dirs::home_dir)
}

fn migrated_expand_env_value(
    raw_value: &str,
    profile_env: &HashMap<String, String>,
    depth: usize,
) -> Option<String> {
    if depth > 16 {
        return None;
    }
    let value = if raw_value == "~" {
        path_to_string(migrated_home_dir()?)
    } else if let Some(rest) = raw_value.strip_prefix("~/") {
        path_to_string(migrated_home_dir()?.join(rest))
    } else {
        raw_value.to_string()
    };
    let mut expanded = String::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '$' {
            expanded.push(ch);
            continue;
        }
        if chars.peek() == Some(&'{') {
            chars.next();
            let mut expr = String::new();
            for next in chars.by_ref() {
                if next == '}' {
                    break;
                }
                expr.push(next);
            }
            expanded.push_str(&migrated_expand_braced_env(&expr, profile_env, depth + 1)?);
            continue;
        }
        let mut name = String::new();
        while let Some(next) = chars.peek().copied() {
            if next.is_ascii_alphanumeric() || next == '_' {
                name.push(next);
                chars.next();
            } else {
                break;
            }
        }
        if name.is_empty() {
            expanded.push('$');
        } else {
            expanded.push_str(
                &migrated_env_string_inner(&name, profile_env, depth + 1).unwrap_or_default(),
            );
        }
    }
    Some(expanded)
}

fn migrated_expand_braced_env(
    expr: &str,
    profile_env: &HashMap<String, String>,
    depth: usize,
) -> Option<String> {
    if let Some((name, fallback)) = expr.split_once(":-") {
        let name = name.trim();
        if let Some(value) = migrated_env_string_inner(name, profile_env, depth + 1)
            && !value.is_empty()
        {
            return Some(value);
        }
        return migrated_expand_env_value(fallback, profile_env, depth + 1);
    }
    let name = expr.trim();
    Some(migrated_env_string_inner(name, profile_env, depth + 1).unwrap_or_default())
}

fn strip_shell_quotes(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        return value[1..value.len() - 1].to_string();
    }
    value.to_string()
}

fn path_to_string(path: std::path::PathBuf) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;
    use std::ffi::OsString;
    use std::sync::{LazyLock, Mutex, MutexGuard};

    use super::{
        validate_migrated_launchd_entrypoint, validate_migrated_launchd_required_connectors,
        validate_migrated_launchd_required_paths,
    };

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner())
    }

    const MIGRATED_ENV_TEST_VARS: &[&str] = &[
        "HOME",
        "USERPROFILE",
        "AGENTDESK_SOURCE_ZPROFILE",
        "AGENTDESK_ROOT_DIR",
        "AGENTDESK_WORKSPACE_ROOT",
        "AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR",
        "OBSIDIAN_VAULT_ROOT",
        "OBSIDIAN_REMOTE_VAULT_ROOT",
        "AGENTDESK_OBSIDIAN_AGENTS_SRC",
        "AGENTDESK_OBSIDIAN_SKILL_ROOT",
        "AGENTDESK_AGENT_FEEDBACK_INBOX",
        "AGENTDESK_MEMORY_MERGE_SKILL",
    ];

    fn snapshot_migrated_env() -> Vec<(&'static str, Option<OsString>)> {
        MIGRATED_ENV_TEST_VARS
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect()
    }

    fn restore_migrated_env(snapshot: Vec<(&'static str, Option<OsString>)>) {
        for (name, value) in snapshot {
            match value {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }

    #[test]
    fn migrated_launchd_entrypoint_validation_uses_repo_relative_fallback() {
        validate_migrated_launchd_entrypoint("migrated-launchd/memory-merge.js", None, &[])
            .expect("repo checkout includes migrated shell entrypoint");
    }

    #[test]
    fn migrated_launchd_entrypoint_validation_uses_metadata_override() {
        let metadata = json!({
            "migrated_launchd": {
                "entrypoint": "scripts/queue-stability-batch.sh"
            }
        });

        validate_migrated_launchd_entrypoint(
            "migrated-launchd/queue-stability-batch.js",
            Some(&metadata),
            &[],
        )
        .expect("queue stability routine uses a repo-local non-launchd-migrated entrypoint");
    }

    #[test]
    fn migrated_launchd_entrypoint_validation_uses_configured_routine_dirs() {
        let temp = tempfile::tempdir().unwrap();
        let routine_dir = temp.path().join("routines");
        let entrypoint_dir = temp.path().join("scripts").join("launchd-migrated");
        std::fs::create_dir_all(&routine_dir).unwrap();
        std::fs::create_dir_all(&entrypoint_dir).unwrap();
        std::fs::write(
            entrypoint_dir.join("operator-only-test-entrypoint.sh"),
            "#!/bin/sh\n",
        )
        .unwrap();
        let metadata = json!({
            "migrated_launchd": {
                "entrypoint": "scripts/launchd-migrated/operator-only-test-entrypoint.sh"
            }
        });

        validate_migrated_launchd_entrypoint(
            "migrated-launchd/operator-only-test-entrypoint.js",
            Some(&metadata),
            &[routine_dir],
        )
        .expect("entrypoint beside configured routine directory should be accepted");
    }

    #[test]
    fn migrated_launchd_required_path_validation_rejects_missing_path() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing-vault");
        let checkpoint = json!({
            "migrated_launchd": {
                "required_paths": [
                    { "label": "operator vault", "path": missing, "kind": "dir" }
                ]
            }
        });

        let err = validate_migrated_launchd_required_paths(
            "migrated-launchd/queue-stability-batch.js",
            None,
            Some(&checkpoint),
        )
        .expect_err("missing required path must block migrated routine enablement");

        assert_eq!(err.status(), StatusCode::CONFLICT);
        assert!(
            err.message()
                .contains("migrated routine migrated-launchd/queue-stability-batch.js is invalid")
        );
        assert!(
            err.message()
                .contains("required directory 'operator vault' not found")
        );
    }

    #[test]
    fn migrated_launchd_required_path_validation_accepts_existing_file() {
        let temp = tempfile::tempdir().unwrap();
        let prompt = temp.path().join("prompt.md");
        std::fs::write(&prompt, "prompt").unwrap();
        let checkpoint = json!({
            "portable": {
                "required_paths": [
                    { "label": "operator prompt", "path": prompt, "kind": "file" },
                    { "label": "optional legacy path", "path": temp.path().join("missing"), "required": false }
                ]
            }
        });

        validate_migrated_launchd_required_paths(
            "migrated-launchd/queue-stability-batch.js",
            None,
            Some(&checkpoint),
        )
        .expect("existing required file should pass");
    }

    #[test]
    fn migrated_launchd_required_path_validation_uses_zprofile_env() {
        let _lock = env_lock();
        let snapshot = snapshot_migrated_env();
        let temp = tempfile::tempdir().unwrap();
        let skill_root = temp.path().join("profile-skills");
        let custom = skill_root.join("custom");
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(custom.join("SKILL.md"), "# Custom\n").unwrap();
        std::fs::write(
            temp.path().join(".zprofile"),
            format!(
                "export AGENTDESK_OBSIDIAN_SKILL_ROOT=\"{}\"\n",
                skill_root.display()
            ),
        )
        .unwrap();
        unsafe {
            std::env::set_var("HOME", temp.path());
            std::env::remove_var("USERPROFILE");
            std::env::remove_var("AGENTDESK_SOURCE_ZPROFILE");
            std::env::remove_var("AGENTDESK_OBSIDIAN_SKILL_ROOT");
        }
        let checkpoint = json!({
            "migrated_launchd": {
                "required_paths": [
                    {
                        "label": "custom skill",
                        "path": "$AGENTDESK_OBSIDIAN_SKILL_ROOT/custom/SKILL.md",
                        "kind": "file"
                    }
                ]
            }
        });

        validate_migrated_launchd_required_paths(
            "migrated-launchd/queue-stability-batch.js",
            None,
            Some(&checkpoint),
        )
        .expect("zprofile-only migrated env should satisfy required path validation");

        restore_migrated_env(snapshot);
    }

    #[test]
    fn migrated_launchd_builtin_paths_require_job_specific_skill_files() {
        let _lock = env_lock();
        let snapshot = snapshot_migrated_env();
        let temp = tempfile::tempdir().unwrap();
        let unrelated = temp.path().join("other-skill");
        std::fs::create_dir_all(&unrelated).unwrap();
        std::fs::write(unrelated.join("SKILL.md"), "# Other\n").unwrap();
        unsafe {
            std::env::set_var("AGENTDESK_OBSIDIAN_SKILL_ROOT", temp.path());
            std::env::set_var("AGENTDESK_SOURCE_ZPROFILE", "0");
        }

        let err = validate_migrated_launchd_required_paths(
            "migrated-launchd/ai-integrated-briefing.js",
            None,
            None,
        )
        .expect_err("coarse skill root readiness must not satisfy job-specific skill");
        assert!(
            err.message().contains("ai-integrated-briefing skill"),
            "{}",
            err.message()
        );

        let skill = temp.path().join("ai-integrated-briefing");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "# AI\n").unwrap();
        validate_migrated_launchd_required_paths(
            "migrated-launchd/ai-integrated-briefing.js",
            None,
            None,
        )
        .expect("job-specific skill should satisfy builtin validation");

        restore_migrated_env(snapshot);
    }

    #[test]
    fn migrated_launchd_builtin_paths_require_banchan_messages() {
        let _lock = env_lock();
        let snapshot = snapshot_migrated_env();
        let temp = tempfile::tempdir().unwrap();
        let banchan = temp.path().join("banchan-day-reminder");
        std::fs::create_dir_all(&banchan).unwrap();
        std::fs::write(banchan.join("SKILL.md"), "# Banchan\n").unwrap();
        unsafe {
            std::env::set_var("AGENTDESK_OBSIDIAN_SKILL_ROOT", temp.path());
            std::env::set_var("AGENTDESK_SOURCE_ZPROFILE", "0");
        }

        let err = validate_migrated_launchd_required_paths(
            "migrated-launchd/banchan-day-reminder-prep.js",
            None,
            None,
        )
        .expect_err("banchan migrated jobs need the messages reference");
        assert!(
            err.message().contains("banchan-day-reminder messages"),
            "{}",
            err.message()
        );

        let references = banchan.join("references");
        std::fs::create_dir_all(&references).unwrap();
        std::fs::write(references.join("messages.md"), "# Messages\n").unwrap();
        validate_migrated_launchd_required_paths(
            "migrated-launchd/banchan-day-reminder-prep.js",
            None,
            None,
        )
        .expect("banchan skill and messages should satisfy builtin validation");

        restore_migrated_env(snapshot);
    }

    #[test]
    fn migrated_launchd_builtin_workdir_honors_workspace_root() {
        let _lock = env_lock();
        let temp = tempfile::tempdir().unwrap();
        let workspace_root = temp.path().join("workspaces-custom");
        let agentfactory = workspace_root.join("agentfactory");
        std::fs::create_dir_all(&agentfactory).unwrap();
        let previous_workspace_root = std::env::var_os("AGENTDESK_WORKSPACE_ROOT");
        let previous_agentfactory = std::env::var_os("AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR");
        unsafe {
            std::env::set_var("AGENTDESK_WORKSPACE_ROOT", &workspace_root);
            std::env::remove_var("AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR");
        }

        validate_migrated_launchd_required_paths(
            "migrated-launchd/memento-daily-report.js",
            None,
            None,
        )
        .expect("AGENTDESK_WORKSPACE_ROOT/agentfactory should satisfy builtin validation");

        match previous_workspace_root {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_WORKSPACE_ROOT", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_WORKSPACE_ROOT") },
        }
        match previous_agentfactory {
            Some(value) => unsafe {
                std::env::set_var("AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR", value)
            },
            None => unsafe { std::env::remove_var("AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR") },
        }
    }

    #[test]
    fn migrated_launchd_required_connector_validation_blocks_missing_connector() {
        let _lock = env_lock();
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing-skills");
        let previous = std::env::var_os("AGENTDESK_OBSIDIAN_SKILL_ROOT");
        unsafe { std::env::set_var("AGENTDESK_OBSIDIAN_SKILL_ROOT", &missing) };
        let metadata = json!({
            "migrated_launchd": {
                "required_connectors": ["obsidian_skill_root"]
            }
        });

        let err = validate_migrated_launchd_required_connectors(
            "migrated-launchd/ai-integrated-briefing.js",
            Some(&metadata),
            None,
        )
        .expect_err("missing required connector must block enablement");

        match previous {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_OBSIDIAN_SKILL_ROOT", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_OBSIDIAN_SKILL_ROOT") },
        }
        assert_eq!(err.status(), StatusCode::CONFLICT);
        assert!(
            err.message()
                .contains("required connector 'obsidian_skill_root' is missing_path")
        );
    }

    #[test]
    fn migrated_launchd_required_connector_validation_accepts_ready_connector() {
        let _lock = env_lock();
        let temp = tempfile::tempdir().unwrap();
        let skill = temp.path().join("ai-integrated-briefing");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "# AI integrated briefing\n").unwrap();
        let previous = std::env::var_os("AGENTDESK_OBSIDIAN_SKILL_ROOT");
        unsafe { std::env::set_var("AGENTDESK_OBSIDIAN_SKILL_ROOT", temp.path()) };
        let metadata = json!({
            "migrated_launchd": {
                "required_connectors": [
                    { "id": "obsidian_skill_root", "label": "Obsidian skills" }
                ]
            }
        });

        validate_migrated_launchd_required_connectors(
            "migrated-launchd/ai-integrated-briefing.js",
            Some(&metadata),
            None,
        )
        .expect("ready connector should pass validation");

        match previous {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_OBSIDIAN_SKILL_ROOT", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_OBSIDIAN_SKILL_ROOT") },
        }
    }

    #[test]
    fn migrated_launchd_required_connector_validation_uses_zprofile_skill_root() {
        let _lock = env_lock();
        let snapshot = snapshot_migrated_env();
        let temp = tempfile::tempdir().unwrap();
        let skill_root = temp.path().join("profile-skills");
        let skill = skill_root.join("ai-integrated-briefing");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), "# AI\n").unwrap();
        std::fs::write(
            temp.path().join(".zprofile"),
            format!(
                "export AGENTDESK_OBSIDIAN_SKILL_ROOT=\"{}\"\n",
                skill_root.display()
            ),
        )
        .unwrap();
        unsafe {
            std::env::set_var("HOME", temp.path());
            std::env::remove_var("USERPROFILE");
            std::env::remove_var("AGENTDESK_SOURCE_ZPROFILE");
            std::env::remove_var("AGENTDESK_OBSIDIAN_SKILL_ROOT");
        }
        let metadata = json!({
            "migrated_launchd": {
                "required_connectors": ["obsidian_skill_root"]
            }
        });

        validate_migrated_launchd_required_connectors(
            "migrated-launchd/ai-integrated-briefing.js",
            Some(&metadata),
            None,
        )
        .expect("zprofile-only skill root should satisfy migrated connector validation");

        restore_migrated_env(snapshot);
    }
}
