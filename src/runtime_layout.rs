use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

pub const MEMORY_LAYOUT_VERSION: u32 = 2;
const DEFAULT_MEMORY_BACKEND: &str = "auto";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryBackendConfig {
    #[serde(default = "default_memory_layout_version")]
    pub version: u32,
    #[serde(default = "default_memory_backend")]
    pub backend: String,
    #[serde(default)]
    pub file: FileMemoryBackendConfig,
    #[serde(default)]
    pub mcp: McpMemoryBackendConfig,
    #[serde(default, rename = "sak_path", skip_serializing)]
    legacy_sak_path: Option<String>,
    #[serde(default, rename = "sam_path", skip_serializing)]
    legacy_sam_path: Option<String>,
    #[serde(default, rename = "ltm_root", skip_serializing)]
    legacy_ltm_root: Option<String>,
}

impl Default for MemoryBackendConfig {
    fn default() -> Self {
        Self {
            version: default_memory_layout_version(),
            backend: default_memory_backend(),
            file: FileMemoryBackendConfig::default(),
            mcp: McpMemoryBackendConfig::default(),
            legacy_sak_path: None,
            legacy_sam_path: None,
            legacy_ltm_root: None,
        }
    }
}

impl MemoryBackendConfig {
    fn normalized(mut self) -> Self {
        self.backend = normalize_memory_backend_name(Some(&self.backend));
        self.file = self.file.normalized(
            self.legacy_sak_path.take(),
            self.legacy_sam_path.take(),
            self.legacy_ltm_root.take(),
        );
        self
    }

    fn with_defaults(mut self) -> Self {
        self.backend = normalize_memory_backend_name(Some(&self.backend));
        self.file = self.file.with_defaults();
        self
    }

    fn with_managed_layout_defaults(mut self) -> Self {
        self.backend = normalize_memory_backend_name(Some(&self.backend));
        self.file = self.file.with_managed_layout_defaults();
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileMemoryBackendConfig {
    #[serde(default = "default_sak_path")]
    pub sak_path: String,
    #[serde(default = "default_sam_path")]
    pub sam_path: String,
    #[serde(default = "default_ltm_root")]
    pub ltm_root: String,
    #[serde(default = "default_auto_memory_root")]
    pub auto_memory_root: String,
}

impl Default for FileMemoryBackendConfig {
    fn default() -> Self {
        Self {
            sak_path: default_sak_path(),
            sam_path: default_sam_path(),
            ltm_root: default_ltm_root(),
            auto_memory_root: default_auto_memory_root(),
        }
    }
}

impl FileMemoryBackendConfig {
    fn normalized(
        mut self,
        legacy_sak_path: Option<String>,
        legacy_sam_path: Option<String>,
        legacy_ltm_root: Option<String>,
    ) -> Self {
        self.sak_path =
            normalize_file_memory_path(self.sak_path, legacy_sak_path, default_sak_path);
        self.sam_path =
            normalize_file_memory_path(self.sam_path, legacy_sam_path, default_sam_path);
        self.ltm_root =
            normalize_file_memory_path(self.ltm_root, legacy_ltm_root, default_ltm_root);
        if self.auto_memory_root.trim().is_empty() {
            self.auto_memory_root = default_auto_memory_root();
        }
        self
    }

    fn with_defaults(mut self) -> Self {
        if self.sak_path.trim().is_empty() {
            self.sak_path = default_sak_path();
        }
        if self.sam_path.trim().is_empty() {
            self.sam_path = default_sam_path();
        }
        if self.ltm_root.trim().is_empty() {
            self.ltm_root = default_ltm_root();
        }
        if self.auto_memory_root.trim().is_empty() {
            self.auto_memory_root = default_auto_memory_root();
        }
        self
    }

    fn with_managed_layout_defaults(mut self) -> Self {
        self.sak_path = default_sak_path();
        self.sam_path = default_sam_path();
        self.ltm_root = default_ltm_root();
        if self.auto_memory_root.trim().is_empty() {
            self.auto_memory_root = default_auto_memory_root();
        }
        self
    }
}

fn normalize_file_memory_path(
    current: String,
    legacy: Option<String>,
    default_value: fn() -> String,
) -> String {
    let current_trimmed = current.trim();
    let default_path = default_value();
    let legacy = legacy.filter(|value| !value.trim().is_empty());

    if current_trimmed.is_empty() {
        return legacy.unwrap_or(default_path);
    }

    if current == default_path {
        return legacy.unwrap_or(default_path);
    }

    current
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpMemoryBackendConfig {
    pub endpoint: String,
    pub access_key_env: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LayoutReport {
    pub migrated: bool,
    pub backup_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillSyncReport {
    pub created_links: usize,
    pub updated_links: usize,
    pub skipped_existing: usize,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SkillManifest {
    #[serde(default = "default_skill_manifest_version")]
    #[allow(dead_code)]
    version: u32,
    #[serde(default)]
    global_core_skills: Vec<String>,
    #[serde(default)]
    skills: BTreeMap<String, SkillManifestEntry>,
    #[serde(flatten)]
    legacy_skills: BTreeMap<String, LegacySkillManifestEntry>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SkillManifestEntry {
    #[serde(default)]
    providers: Vec<String>,
    #[serde(default)]
    workspaces: Vec<String>,
    #[serde(default)]
    global: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct LegacySkillManifestEntry {
    #[serde(default)]
    targets: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SkillDeploymentPlan {
    global_providers: Vec<String>,
    workspace_targets: Vec<WorkspaceSkillTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceSkillTarget {
    root: PathBuf,
    providers: Vec<String>,
}

fn default_memory_layout_version() -> u32 {
    MEMORY_LAYOUT_VERSION
}

fn default_memory_backend() -> String {
    DEFAULT_MEMORY_BACKEND.to_string()
}

fn default_sak_path() -> String {
    "memories/shared-agent-knowledge/shared_knowledge.md".to_string()
}

fn default_sam_path() -> String {
    "memories/shared-agent-memory".to_string()
}

fn default_ltm_root() -> String {
    "memories/long-term".to_string()
}

fn default_auto_memory_root() -> String {
    "~/.claude/projects/*{workspace}*/memory/".to_string()
}

fn default_skill_manifest_version() -> u32 {
    1
}

fn normalize_memory_backend_name(raw: Option<&str>) -> String {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None => DEFAULT_MEMORY_BACKEND.to_string(),
        Some(value) if value.eq_ignore_ascii_case("auto") => "auto".to_string(),
        Some(value) if value.eq_ignore_ascii_case("file") => "file".to_string(),
        Some(value) if value.eq_ignore_ascii_case("local") => "file".to_string(),
        Some(value) if value.eq_ignore_ascii_case("memento") => "memento".to_string(),
        Some(value) if value.eq_ignore_ascii_case("mem0") => "mem0".to_string(),
        Some(_) => DEFAULT_MEMORY_BACKEND.to_string(),
    }
}

fn current_home_dir() -> Option<PathBuf> {
    #[cfg(test)]
    {
        if let Ok(slot) = test_home_dir_override_slot().lock() {
            if let Some(override_path) = slot.clone() {
                return override_path;
            }
        }
    }
    dirs::home_dir()
}

fn should_scan_global_skill_targets(root: &Path) -> bool {
    crate::config::runtime_root()
        .map(|active_root| same_canonical_path(root, &active_root))
        .unwrap_or(false)
}

#[cfg(test)]
fn test_home_dir_override_slot() -> &'static std::sync::Mutex<Option<Option<PathBuf>>> {
    static OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<Option<PathBuf>>>> =
        std::sync::OnceLock::new();
    OVERRIDE.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
fn set_test_home_dir_override(path: Option<PathBuf>) {
    if let Ok(mut slot) = test_home_dir_override_slot().lock() {
        *slot = Some(path);
    }
}

#[cfg(test)]
fn clear_test_home_dir_override() {
    if let Ok(mut slot) = test_home_dir_override_slot().lock() {
        *slot = None;
    }
}

pub fn config_dir(root: &Path) -> PathBuf {
    root.join("config")
}

pub fn config_file_path(root: &Path) -> PathBuf {
    config_dir(root).join("agentdesk.yaml")
}

pub fn legacy_config_file_path(root: &Path) -> PathBuf {
    root.join("agentdesk.yaml")
}

pub fn role_map_path(root: &Path) -> PathBuf {
    config_dir(root).join("role_map.json")
}

pub fn org_schema_path(root: &Path) -> PathBuf {
    config_dir(root).join("org.yaml")
}

pub fn memory_backend_path(root: &Path) -> PathBuf {
    config_dir(root).join("memory-backend.json")
}

pub fn managed_agents_root(root: &Path) -> PathBuf {
    config_dir(root).join("agents")
}

pub fn managed_memories_root(root: &Path) -> PathBuf {
    config_dir(root).join("memories")
}

pub fn shared_agent_knowledge_dir(root: &Path) -> PathBuf {
    managed_memories_root(root).join("shared-agent-knowledge")
}

fn default_shared_agent_knowledge_path(root: &Path) -> PathBuf {
    shared_agent_knowledge_dir(root).join("shared_knowledge.md")
}

pub fn shared_agent_knowledge_path(root: &Path) -> PathBuf {
    resolve_memory_path(root, &load_memory_backend(root).file.sak_path)
}

fn default_shared_agent_memory_root(root: &Path) -> PathBuf {
    managed_memories_root(root).join("shared-agent-memory")
}

#[allow(dead_code)]
pub fn shared_agent_memory_root(root: &Path) -> PathBuf {
    resolve_memory_path(root, &load_memory_backend(root).file.sam_path)
}

fn default_long_term_memory_root(root: &Path) -> PathBuf {
    managed_memories_root(root).join("long-term")
}

pub fn long_term_memory_root(root: &Path) -> PathBuf {
    resolve_memory_path(root, &load_memory_backend(root).file.ltm_root)
}

pub fn memories_archive_root(root: &Path) -> PathBuf {
    managed_memories_root(root).join("archive")
}

pub fn managed_skills_root(root: &Path) -> PathBuf {
    root.join("skills")
}

pub fn managed_skills_manifest_path(root: &Path) -> PathBuf {
    managed_skills_root(root).join("manifest.json")
}

pub fn resolve_memory_path(root: &Path, raw: &str) -> PathBuf {
    let raw_path = expand_user_path(raw).unwrap_or_else(|| PathBuf::from(raw));
    if raw_path.is_absolute() {
        raw_path
    } else {
        config_dir(root).join(raw_path)
    }
}

pub fn load_memory_backend(root: &Path) -> MemoryBackendConfig {
    let candidates = [memory_backend_path(root), root.join("memory-backend.json")];
    for path in candidates {
        if let Ok(content) = fs::read_to_string(&path) {
            if let Ok(config) = serde_json::from_str::<MemoryBackendConfig>(&content) {
                return config.normalized();
            }
        }
    }
    MemoryBackendConfig::default()
}

pub fn ensure_runtime_layout(root: &Path) -> Result<LayoutReport, String> {
    fs::create_dir_all(root).map_err(|e| format!("Failed to create '{}': {e}", root.display()))?;

    let mut report = LayoutReport::default();
    let needs_migration = legacy_layout_needs_migration(root);
    if needs_migration {
        report.backup_path = Some(create_legacy_backup(root)?);
        migrate_legacy_layout(root)?;
        report.migrated = true;
    }

    ensure_layout_dirs(root)?;
    ensure_managed_skills_manifest(root)?;
    migrate_legacy_skill_links(root)?;
    write_memory_backend(root)?;
    Ok(report)
}

pub fn sync_managed_skills(root: &Path) -> Result<SkillSyncReport, String> {
    ensure_managed_skills_manifest(root)?;
    let skills_root = managed_skills_root(root);
    if !skills_root.is_dir() {
        return Ok(SkillSyncReport::default());
    }

    let manifest = load_skill_manifest(root)?;
    let skill_dirs = discover_skill_dirs(root)?;
    if skill_dirs.is_empty() {
        return Ok(SkillSyncReport::default());
    }

    let workspace_names = discover_workspaces(root)?;
    let mut report = SkillSyncReport::default();

    for (skill_name, skill_dir) in skill_dirs {
        let deployment = skill_deployment_plan(&manifest, &skill_name, root, &workspace_names);

        if !deployment.global_providers.is_empty() {
            let Some(home) = current_home_dir() else {
                return Ok(report);
            };
            for provider in &deployment.global_providers {
                let target_dir = provider_target_dir(provider, &home);
                match deploy_skill_link(provider, &skill_name, &skill_dir, &target_dir) {
                    Ok(LinkState::Created) => report.created_links += 1,
                    Ok(LinkState::Updated) => report.updated_links += 1,
                    Ok(LinkState::SkippedExisting) => report.skipped_existing += 1,
                    Ok(LinkState::Unchanged) => {}
                    Err(err) => {
                        return Err(format!(
                            "Failed to deploy global skill '{}' for {}: {}",
                            skill_name, provider, err
                        ));
                    }
                }
            }
        }

        for workspace_target in deployment.workspace_targets {
            if !workspace_target.root.is_dir() {
                continue;
            }
            let workspace_label = workspace_target.root.display().to_string();
            for provider in &workspace_target.providers {
                let target_dir = provider_target_dir(provider, &workspace_target.root);
                match deploy_skill_link(provider, &skill_name, &skill_dir, &target_dir) {
                    Ok(LinkState::Created) => report.created_links += 1,
                    Ok(LinkState::Updated) => report.updated_links += 1,
                    Ok(LinkState::SkippedExisting) => report.skipped_existing += 1,
                    Ok(LinkState::Unchanged) => {}
                    Err(err) => {
                        return Err(format!(
                            "Failed to deploy workspace skill '{}' to {} ({}) : {}",
                            skill_name, workspace_label, provider, err
                        ));
                    }
                }
            }
        }
    }

    Ok(report)
}

fn legacy_layout_needs_migration(root: &Path) -> bool {
    let backend = load_memory_backend(root);
    if backend.version < MEMORY_LAYOUT_VERSION {
        return true;
    }

    legacy_config_file_path(root).exists()
        || root.join("memory-backend.json").exists()
        || path_exists(&config_dir(root).join("role-context"))
        || path_exists(&root.join("role-context"))
        || path_exists(&config_dir(root).join("long-term-memory"))
        || path_exists(&root.join("long-term-memory"))
        || path_exists(&config_dir(root).join("shared_agent_memory"))
        || path_exists(&root.join("shared_agent_memory"))
}

fn ensure_layout_dirs(root: &Path) -> Result<(), String> {
    let backend = load_memory_backend(root).with_defaults();
    for dir in [
        config_dir(root),
        managed_agents_root(root),
        shared_agent_knowledge_path(root)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| shared_agent_knowledge_dir(root)),
        resolve_memory_path(root, &backend.file.sam_path),
        resolve_memory_path(root, &backend.file.ltm_root),
        memories_archive_root(root),
        managed_skills_root(root),
    ] {
        fs::create_dir_all(&dir)
            .map_err(|e| format!("Failed to create '{}': {e}", dir.display()))?;
    }
    Ok(())
}

fn write_memory_backend(root: &Path) -> Result<(), String> {
    let path = memory_backend_path(root);
    let mut config = load_memory_backend(root).with_defaults();
    config.version = MEMORY_LAYOUT_VERSION;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create '{}': {e}", parent.display()))?;
    }
    let rendered = serde_json::to_string_pretty(&config)
        .map_err(|e| format!("Failed to serialize '{}': {e}", path.display()))?;
    let needs_write = fs::read_to_string(&path)
        .map(|existing| existing != rendered)
        .unwrap_or(true);
    if needs_write {
        fs::write(&path, rendered)
            .map_err(|e| format!("Failed to write '{}': {e}", path.display()))?;
    }
    Ok(())
}

fn ensure_managed_skills_manifest(root: &Path) -> Result<(), String> {
    let skills_root = managed_skills_root(root);
    fs::create_dir_all(&skills_root)
        .map_err(|e| format!("Failed to create '{}': {e}", skills_root.display()))?;
    let manifest_path = managed_skills_manifest_path(root);
    if manifest_path.exists() {
        return Ok(());
    }

    let rendered = serde_json::to_string_pretty(&serde_json::json!({
        "version": 1,
        "global_core_skills": [],
        "skills": {}
    }))
    .map_err(|e| format!("Failed to serialize '{}': {e}", manifest_path.display()))?;
    fs::write(&manifest_path, rendered)
        .map_err(|e| format!("Failed to write '{}': {e}", manifest_path.display()))
}

fn create_legacy_backup(root: &Path) -> Result<PathBuf, String> {
    let config_link = config_dir(root);
    let backup_parent = resolved_existing_dir(&config_link)
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| root.to_path_buf());
    let backup_root = backup_parent.join("config.backup-v1");
    if path_exists(&backup_root) {
        return Ok(backup_root);
    }
    fs::create_dir_all(&backup_root)
        .map_err(|e| format!("Failed to create '{}': {e}", backup_root.display()))?;

    if path_exists(&config_link) {
        copy_path_resolving_symlinks(&config_link, &backup_root.join("config"))?;
    }
    for legacy in [
        root.join("shared_agent_memory"),
        config_dir(root).join("shared_agent_memory"),
    ] {
        if path_exists(&legacy) {
            let name = legacy
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("shared_agent_memory");
            copy_path_resolving_symlinks(&legacy, &backup_root.join(name))?;
        }
    }
    let legacy_yaml = legacy_config_file_path(root);
    if legacy_yaml.is_file() {
        copy_path_resolving_symlinks(&legacy_yaml, &backup_root.join("agentdesk.yaml"))?;
    }
    let legacy_memory_backend = root.join("memory-backend.json");
    if legacy_memory_backend.is_file() {
        copy_path_resolving_symlinks(
            &legacy_memory_backend,
            &backup_root.join("memory-backend.json"),
        )?;
    }
    Ok(backup_root)
}

fn migrate_legacy_layout(root: &Path) -> Result<(), String> {
    migrate_legacy_config_file(root)?;
    migrate_memory_backend_file(root)?;
    migrate_role_context(root)?;
    migrate_shared_agent_memory(root)?;
    update_role_map_prompt_paths(root)?;
    update_org_yaml_prompt_paths(root)?;
    Ok(())
}

fn migrate_legacy_config_file(root: &Path) -> Result<(), String> {
    let legacy = legacy_config_file_path(root);
    let current = config_file_path(root);
    if !legacy.is_file() {
        return Ok(());
    }
    if !current.exists() {
        if let Some(parent) = current.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create '{}': {e}", parent.display()))?;
        }
        copy_path_resolving_symlinks(&legacy, &current)?;
    }
    fs::remove_file(&legacy).map_err(|e| format!("Failed to remove '{}': {e}", legacy.display()))
}

fn migrate_memory_backend_file(root: &Path) -> Result<(), String> {
    let legacy = root.join("memory-backend.json");
    let current = memory_backend_path(root);
    let mut backend = load_memory_backend(root);
    backend = if backend.version < MEMORY_LAYOUT_VERSION {
        backend.with_managed_layout_defaults()
    } else {
        backend.with_defaults()
    };
    backend.version = MEMORY_LAYOUT_VERSION;

    if let Some(parent) = current.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create '{}': {e}", parent.display()))?;
    }
    let rendered = serde_json::to_string_pretty(&backend)
        .map_err(|e| format!("Failed to serialize '{}': {e}", current.display()))?;
    fs::write(&current, rendered)
        .map_err(|e| format!("Failed to write '{}': {e}", current.display()))?;
    if legacy.is_file() && legacy != current {
        fs::remove_file(&legacy)
            .map_err(|e| format!("Failed to remove '{}': {e}", legacy.display()))?;
    }
    Ok(())
}

fn migrate_role_context(root: &Path) -> Result<(), String> {
    let dest_agents = managed_agents_root(root);
    let dest_ltm = default_long_term_memory_root(root);
    fs::create_dir_all(&dest_agents)
        .map_err(|e| format!("Failed to create '{}': {e}", dest_agents.display()))?;
    fs::create_dir_all(&dest_ltm)
        .map_err(|e| format!("Failed to create '{}': {e}", dest_ltm.display()))?;

    let mut seen = BTreeSet::new();
    let candidates = [
        config_dir(root).join("role-context"),
        root.join("role-context"),
        config_dir(root).join("long-term-memory"),
        root.join("long-term-memory"),
    ];

    for source in candidates {
        let display = source.display().to_string();
        if !path_exists(&source) || !seen.insert(display) {
            continue;
        }
        if source
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == "long-term-memory")
        {
            copy_dir_entries_resolving_symlinks(&source, &dest_ltm)?;
            remove_legacy_path(&source)?;
            continue;
        }

        copy_agent_entries_resolving_symlinks(&source, &dest_agents)?;
        copy_role_context_memory_dirs(&source, &dest_ltm)?;
        remove_legacy_path(&source)?;
    }
    Ok(())
}

fn copy_agent_entries_resolving_symlinks(src: &Path, dest_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dest_dir)
        .map_err(|e| format!("Failed to create '{}': {e}", dest_dir.display()))?;
    for entry in read_dir_resolved(src)? {
        let Some(name) = entry.file_name() else {
            continue;
        };
        let name_str = name.to_string_lossy();
        if name_str.ends_with(".memory") {
            continue;
        }
        copy_path_resolving_symlinks(&entry, &dest_dir.join(name))?;
    }
    Ok(())
}

fn copy_role_context_memory_dirs(source: &Path, dest_ltm: &Path) -> Result<(), String> {
    let entries = read_dir_resolved(source)?;
    for entry in entries {
        let name = entry
            .file_name()
            .and_then(|value| value.to_str().map(ToString::to_string))
            .unwrap_or_default();
        if !name.ends_with(".memory") {
            continue;
        }
        let role_id = name.trim_end_matches(".memory");
        if role_id.is_empty() {
            continue;
        }
        copy_path_resolving_symlinks(&entry, &dest_ltm.join(role_id))?;
    }
    Ok(())
}

fn migrate_shared_agent_memory(root: &Path) -> Result<(), String> {
    let dest_knowledge = default_shared_agent_knowledge_path(root);
    let dest_sam_root = default_shared_agent_memory_root(root);
    let dest_archive = memories_archive_root(root);

    if let Some(parent) = dest_knowledge.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create '{}': {e}", parent.display()))?;
    }
    fs::create_dir_all(&dest_sam_root)
        .map_err(|e| format!("Failed to create '{}': {e}", dest_sam_root.display()))?;
    fs::create_dir_all(&dest_archive)
        .map_err(|e| format!("Failed to create '{}': {e}", dest_archive.display()))?;

    let mut seen = BTreeSet::new();
    for source in [
        root.join("shared_agent_memory"),
        config_dir(root).join("shared_agent_memory"),
    ] {
        let display = source.display().to_string();
        if !path_exists(&source) || !seen.insert(display) {
            continue;
        }
        for entry in read_dir_resolved(&source)? {
            let file_name = entry
                .file_name()
                .and_then(|value| value.to_str().map(ToString::to_string))
                .unwrap_or_default();
            if file_name == "shared_knowledge.md" {
                copy_path_resolving_symlinks(&entry, &dest_knowledge)?;
                continue;
            }
            if file_name == "archive" {
                copy_dir_entries_resolving_symlinks(&entry, &dest_archive)?;
                continue;
            }
            if entry
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            {
                copy_path_resolving_symlinks(&entry, &dest_sam_root.join(file_name))?;
            }
        }
        remove_legacy_path(&source)?;
    }
    Ok(())
}

fn update_role_map_prompt_paths(root: &Path) -> Result<(), String> {
    let path = role_map_path(root);
    if !path.is_file() {
        return Ok(());
    }
    let content = fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
    let mut json: Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse '{}': {e}", path.display()))?;
    rewrite_prompt_paths_json(&mut json);
    let rendered = serde_json::to_string_pretty(&json)
        .map_err(|e| format!("Failed to serialize '{}': {e}", path.display()))?;
    fs::write(&path, rendered).map_err(|e| format!("Failed to write '{}': {e}", path.display()))
}

fn update_org_yaml_prompt_paths(root: &Path) -> Result<(), String> {
    let path = org_schema_path(root);
    if !path.is_file() {
        return Ok(());
    }
    let content = fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
    let mut yaml: serde_yaml::Value = serde_yaml::from_str(&content)
        .map_err(|e| format!("Failed to parse '{}': {e}", path.display()))?;
    rewrite_prompt_paths_yaml(&mut yaml);
    let rendered = serde_yaml::to_string(&yaml)
        .map_err(|e| format!("Failed to serialize '{}': {e}", path.display()))?;
    fs::write(&path, rendered).map_err(|e| format!("Failed to write '{}': {e}", path.display()))
}

fn rewrite_prompt_paths_json(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if matches!(key.as_str(), "promptFile" | "prompt_file") {
                    if let Some(raw) = child.as_str() {
                        *child = Value::String(rewrite_prompt_path(raw));
                    }
                } else {
                    rewrite_prompt_paths_json(child);
                }
            }
        }
        Value::Array(items) => {
            for child in items {
                rewrite_prompt_paths_json(child);
            }
        }
        _ => {}
    }
}

fn rewrite_prompt_paths_yaml(value: &mut serde_yaml::Value) {
    match value {
        serde_yaml::Value::Mapping(map) => {
            for (key, child) in map.iter_mut() {
                let key_str = key.as_str().unwrap_or_default();
                if matches!(key_str, "promptFile" | "prompt_file") {
                    if let Some(raw) = child.as_str() {
                        *child = serde_yaml::Value::String(rewrite_prompt_path(raw));
                    }
                } else {
                    rewrite_prompt_paths_yaml(child);
                }
            }
        }
        serde_yaml::Value::Sequence(items) => {
            for child in items {
                rewrite_prompt_paths_yaml(child);
            }
        }
        _ => {}
    }
}

fn rewrite_prompt_path(raw: &str) -> String {
    raw.replace("role-context/", "agents/")
        .replace("role-context\\", "agents\\")
}

fn skill_deployment_plan(
    manifest: &SkillManifest,
    skill_name: &str,
    root: &Path,
    workspace_names: &[String],
) -> SkillDeploymentPlan {
    if let Some(entry) = manifest.skills.get(skill_name) {
        let providers = normalize_provider_list(&entry.providers)
            .into_iter()
            .collect::<Vec<_>>();
        let providers = if providers.is_empty() {
            all_skill_providers()
        } else {
            providers
        };
        let global_enabled = entry.global.unwrap_or_else(|| {
            if manifest.skills.is_empty()
                && manifest.global_core_skills.is_empty()
                && manifest.legacy_skills.is_empty()
            {
                true
            } else {
                manifest
                    .global_core_skills
                    .iter()
                    .any(|name| name == skill_name)
            }
        });
        let workspace_targets = if entry.workspaces.is_empty() {
            workspace_targets_for_names(root, workspace_names, &providers)
        } else {
            workspace_targets_for_names(
                root,
                &normalize_workspace_names(&entry.workspaces),
                &providers,
            )
        };
        return SkillDeploymentPlan {
            global_providers: if global_enabled {
                providers.clone()
            } else {
                Vec::new()
            },
            workspace_targets,
        };
    }

    if let Some(entry) = manifest.legacy_skills.get(skill_name) {
        return legacy_skill_deployment_plan(&entry.targets);
    }

    if manifest.skills.is_empty()
        && manifest.global_core_skills.is_empty()
        && manifest.legacy_skills.is_empty()
    {
        let providers = all_skill_providers();
        return SkillDeploymentPlan {
            global_providers: providers.clone(),
            workspace_targets: workspace_targets_for_names(root, workspace_names, &providers),
        };
    }

    if !manifest.legacy_skills.is_empty() {
        return SkillDeploymentPlan {
            global_providers: all_skill_providers(),
            workspace_targets: Vec::new(),
        };
    }

    let providers = all_skill_providers();
    SkillDeploymentPlan {
        global_providers: if manifest
            .global_core_skills
            .iter()
            .any(|name| name == skill_name)
        {
            providers.clone()
        } else {
            Vec::new()
        },
        workspace_targets: workspace_targets_for_names(root, workspace_names, &providers),
    }
}

fn workspace_targets_for_names(
    root: &Path,
    workspace_names: &[String],
    providers: &[String],
) -> Vec<WorkspaceSkillTarget> {
    let mut result = Vec::new();
    for workspace_name in workspace_names {
        let workspace_root = root.join("workspaces").join(workspace_name);
        if !workspace_root.is_dir() {
            continue;
        }
        result.push(WorkspaceSkillTarget {
            root: workspace_root,
            providers: providers.to_vec(),
        });
    }
    result
}

fn legacy_skill_deployment_plan(targets: &[String]) -> SkillDeploymentPlan {
    let mut global = BTreeSet::new();
    let mut workspaces = BTreeMap::<PathBuf, BTreeSet<String>>::new();

    for target in targets {
        match parse_legacy_skill_target(target) {
            Some(LegacySkillTarget::Global { provider }) => {
                global.insert(provider);
            }
            Some(LegacySkillTarget::Workspace { provider, root }) => {
                workspaces.entry(root).or_default().insert(provider);
            }
            None => {}
        }
    }

    SkillDeploymentPlan {
        global_providers: global.into_iter().collect(),
        workspace_targets: workspaces
            .into_iter()
            .map(|(root, providers)| WorkspaceSkillTarget {
                root,
                providers: providers.into_iter().collect(),
            })
            .collect(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LegacySkillTarget {
    Global { provider: String },
    Workspace { provider: String, root: PathBuf },
}

fn parse_legacy_skill_target(raw: &str) -> Option<LegacySkillTarget> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some((provider_raw, root_raw)) = trimmed.split_once('@') {
        let provider = normalize_provider_name(provider_raw)?;
        let root = expand_user_path(root_raw)?;
        return Some(LegacySkillTarget::Workspace { provider, root });
    }
    normalize_provider_name(trimmed).map(|provider| LegacySkillTarget::Global { provider })
}

fn expand_user_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(stripped) = trimmed.strip_prefix("~/") {
        let home = current_home_dir()?;
        return Some(home.join(stripped));
    }
    if trimmed == "~" {
        return current_home_dir();
    }
    Some(PathBuf::from(trimmed))
}

fn load_skill_manifest(root: &Path) -> Result<SkillManifest, String> {
    let path = managed_skills_manifest_path(root);
    let content = fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
    serde_json::from_str(&content).map_err(|e| format!("Failed to parse '{}': {e}", path.display()))
}

fn discover_skill_dirs(root: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let mut result = Vec::new();
    let skills_root = managed_skills_root(root);
    if !skills_root.is_dir() {
        return Ok(result);
    }
    let entries = fs::read_dir(&skills_root)
        .map_err(|e| format!("Failed to read '{}': {e}", skills_root.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if !path.join("SKILL.md").is_file() {
            continue;
        }
        let Some(name) = path
            .file_name()
            .and_then(|value| value.to_str())
            .map(ToString::to_string)
        else {
            continue;
        };
        result.push((name, path));
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(result)
}

fn discover_workspaces(root: &Path) -> Result<Vec<String>, String> {
    let workspaces_root = root.join("workspaces");
    let mut result = Vec::new();
    if !workspaces_root.is_dir() {
        return Ok(result);
    }
    let entries = fs::read_dir(&workspaces_root)
        .map_err(|e| format!("Failed to read '{}': {e}", workspaces_root.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        result.push(name.to_string());
    }
    result.sort();
    Ok(result)
}

fn normalize_provider_list(values: &[String]) -> Vec<String> {
    let mut providers = values
        .iter()
        .filter_map(|value| normalize_provider_name(value))
        .collect::<Vec<_>>();
    providers.sort();
    providers.dedup();
    providers
}

fn normalize_workspace_names(values: &[String]) -> Vec<String> {
    let mut workspaces = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    workspaces.sort();
    workspaces.dedup();
    workspaces
}

fn all_skill_providers() -> Vec<String> {
    vec![
        "claude".to_string(),
        "codex".to_string(),
        "gemini".to_string(),
        "qwen".to_string(),
    ]
}

fn normalize_provider_name(value: &str) -> Option<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "claude" => Some("claude".to_string()),
        "codex" => Some("codex".to_string()),
        "gemini" => Some("gemini".to_string()),
        "qwen" => Some("qwen".to_string()),
        _ => None,
    }
}

fn migrate_legacy_skill_links(root: &Path) -> Result<(), String> {
    let manifest = load_skill_manifest(root)?;
    let mut candidates = collect_legacy_skill_target_dirs(root, &manifest)?;
    candidates.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    candidates.dedup();

    for (provider, target_dir) in candidates {
        if !target_dir.is_dir() {
            continue;
        }
        migrate_legacy_skill_links_for_target(root, &provider, &target_dir)?;
    }
    Ok(())
}

fn collect_legacy_skill_target_dirs(
    root: &Path,
    manifest: &SkillManifest,
) -> Result<Vec<(String, PathBuf)>, String> {
    let mut result = Vec::new();
    if should_scan_global_skill_targets(root) {
        let Some(home) = current_home_dir() else {
            return Ok(result);
        };
        for provider in all_skill_providers() {
            result.push((provider.clone(), provider_target_dir(&provider, &home)));
        }
    }

    for workspace_name in discover_workspaces(root)? {
        let workspace_root = root.join("workspaces").join(workspace_name);
        for provider in all_skill_providers() {
            result.push((
                provider.clone(),
                provider_target_dir(&provider, &workspace_root),
            ));
        }
    }

    for entry in manifest.legacy_skills.values() {
        for target in &entry.targets {
            if let Some(LegacySkillTarget::Workspace { provider, root }) =
                parse_legacy_skill_target(target)
            {
                result.push((provider.clone(), provider_target_dir(&provider, &root)));
            }
        }
    }
    Ok(result)
}

fn migrate_legacy_skill_links_for_target(
    root: &Path,
    provider: &str,
    target_dir: &Path,
) -> Result<(), String> {
    let entries = fs::read_dir(target_dir)
        .map_err(|e| format!("Failed to read '{}': {e}", target_dir.display()))?;
    for entry in entries.flatten() {
        let link_path = entry.path();
        let meta = match fs::symlink_metadata(&link_path) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if !meta.file_type().is_symlink() {
            continue;
        }
        let Some((skill_name, source_skill_dir)) =
            resolve_linked_skill_source(provider, &link_path)
        else {
            continue;
        };
        let managed_skill_dir = ensure_managed_skill_dir(root, &skill_name, &source_skill_dir)?;
        deploy_skill_link(provider, &skill_name, &managed_skill_dir, target_dir)?;
    }
    Ok(())
}

fn resolve_linked_skill_source(provider: &str, link_path: &Path) -> Option<(String, PathBuf)> {
    let resolved = fs::canonicalize(link_path).ok()?;
    let skill_name = if provider == "claude" {
        link_path.file_stem()?.to_str()?.to_string()
    } else {
        link_path.file_name()?.to_str()?.to_string()
    };
    let skill_dir = if resolved.is_dir() {
        resolved
    } else if resolved
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("SKILL.md"))
    {
        resolved.parent()?.to_path_buf()
    } else {
        return None;
    };
    if !skill_dir.join("SKILL.md").is_file() {
        return None;
    }
    Some((skill_name, skill_dir))
}

fn ensure_managed_skill_dir(
    root: &Path,
    skill_name: &str,
    source_skill_dir: &Path,
) -> Result<PathBuf, String> {
    let managed_dir = managed_skills_root(root).join(skill_name);
    if !same_canonical_path(&managed_dir, source_skill_dir)
        && !managed_dir.join("SKILL.md").is_file()
    {
        copy_skill_dir_resolving_symlinks(source_skill_dir, &managed_dir)?;
    }
    rewrite_managed_skill_paths(skill_name, &managed_dir)?;
    Ok(managed_dir)
}

fn same_canonical_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(lhs), Ok(rhs)) => lhs == rhs,
        _ => a == b,
    }
}

fn rewrite_managed_skill_paths(skill_name: &str, skill_dir: &Path) -> Result<(), String> {
    let files = match skill_name {
        "memory-read" | "memory-write" => vec![skill_dir.join("SKILL.md")],
        "memory-merge" => vec![
            skill_dir.join("SKILL.md"),
            skill_dir.join("references").join("architecture.md"),
            skill_dir.join("references").join("classification-guide.md"),
            skill_dir.join("references").join("phase-details.md"),
            skill_dir.join("references").join("report-template.md"),
        ],
        _ => return Ok(()),
    };

    for path in files {
        rewrite_text_file_paths(&path)?;
    }
    Ok(())
}

fn rewrite_text_file_paths(path: &Path) -> Result<(), String> {
    if !path.is_file() {
        return Ok(());
    }
    let original = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
    let mut rewritten = original.clone();
    for (from, to) in skill_path_replacements() {
        rewritten = rewritten.replace(from, to);
    }
    if rewritten != original {
        fs::write(path, rewritten)
            .map_err(|e| format!("Failed to write '{}': {e}", path.display()))?;
    }
    Ok(())
}

fn skill_path_replacements() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "~/.adk/release/shared_agent_memory/shared_knowledge.md",
            "~/.adk/release/config/memories/shared-agent-knowledge/shared_knowledge.md",
        ),
        (
            "~/.adk/release/shared_agent_memory/{role_id}.json",
            "~/.adk/release/config/memories/shared-agent-memory/{role_id}.json",
        ),
        (
            "~/.adk/release/shared_agent_memory/{roleId}.json",
            "~/.adk/release/config/memories/shared-agent-memory/{roleId}.json",
        ),
        (
            "~/.adk/release/shared_agent_memory/archive/{YYYY-MM-DD}/",
            "~/.adk/release/config/memories/archive/{YYYY-MM-DD}/",
        ),
        (
            "~/ObsidianVault/RemoteVault/agents/{role_id}/long-term-memory/",
            "~/.adk/release/config/memories/long-term/{role_id}/",
        ),
        (
            "~/ObsidianVault/RemoteVault/agents/{roleId}/long-term-memory/",
            "~/.adk/release/config/memories/long-term/{roleId}/",
        ),
        (
            "~/.claude/projects/-Users-itismyfield--adk-release-workspaces-{workspace}/memory/",
            "~/.claude/projects/*{workspace}*/memory/",
        ),
    ]
}

fn provider_target_dir(provider: &str, base: &Path) -> PathBuf {
    match provider {
        "claude" => base.join(".claude").join("commands"),
        "codex" => base.join(".codex").join("skills"),
        "gemini" => base.join(".gemini").join("skills"),
        "qwen" => base.join(".qwen").join("skills"),
        _ => base.join(".codex").join("skills"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkState {
    Created,
    Updated,
    Unchanged,
    SkippedExisting,
}

fn deploy_skill_link(
    provider: &str,
    skill_name: &str,
    skill_dir: &Path,
    target_dir: &Path,
) -> Result<LinkState, String> {
    fs::create_dir_all(target_dir)
        .map_err(|e| format!("Failed to create '{}': {e}", target_dir.display()))?;

    let (source_path, link_path, is_dir_link) = if provider == "claude" {
        (
            skill_dir.join("SKILL.md"),
            target_dir.join(format!("{skill_name}.md")),
            false,
        )
    } else {
        (skill_dir.to_path_buf(), target_dir.join(skill_name), true)
    };

    if !path_exists(&source_path) {
        return Ok(LinkState::SkippedExisting);
    }

    if let Ok(existing_target) = fs::read_link(&link_path) {
        let desired = relative_path_from(
            link_path.parent().unwrap_or(target_dir),
            &source_path.canonicalize().unwrap_or(source_path.clone()),
        );
        if existing_target == desired {
            return Ok(LinkState::Unchanged);
        }
        remove_link_or_path(&link_path)?;
        create_symlink_entry(&source_path, &link_path, is_dir_link)?;
        return Ok(LinkState::Updated);
    }

    if path_exists(&link_path) {
        return Ok(LinkState::SkippedExisting);
    }

    create_symlink_entry(&source_path, &link_path, is_dir_link)?;
    Ok(LinkState::Created)
}

fn create_symlink_entry(source: &Path, link_path: &Path, _is_dir_link: bool) -> Result<(), String> {
    let link_parent = link_path
        .parent()
        .ok_or_else(|| format!("Link path '{}' has no parent", link_path.display()))?;
    let target = relative_path_from(
        link_parent,
        &source
            .canonicalize()
            .unwrap_or_else(|_| source.to_path_buf()),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(&target, link_path)
            .map_err(|e| format!("Failed to create symlink '{}': {e}", link_path.display()))
    }
    #[cfg(windows)]
    {
        if _is_dir_link {
            std::os::windows::fs::symlink_dir(&target, link_path).map_err(|e| {
                format!(
                    "Failed to create dir symlink '{}': {e}",
                    link_path.display()
                )
            })
        } else {
            std::os::windows::fs::symlink_file(&target, link_path).map_err(|e| {
                format!(
                    "Failed to create file symlink '{}': {e}",
                    link_path.display()
                )
            })
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = _is_dir_link;
        Err(format!(
            "Symlink deployment is not supported on this platform for '{}'",
            link_path.display()
        ))
    }
}

fn relative_path_from(from_dir: &Path, to_path: &Path) -> PathBuf {
    let from_canonical = from_dir
        .canonicalize()
        .unwrap_or_else(|_| from_dir.to_path_buf());
    let to_canonical = to_path
        .canonicalize()
        .unwrap_or_else(|_| to_path.to_path_buf());
    let from_components = normalize_components(&from_canonical);
    let to_components = normalize_components(&to_canonical);

    let mut common = 0usize;
    while common < from_components.len()
        && common < to_components.len()
        && from_components[common] == to_components[common]
    {
        common += 1;
    }

    let mut relative = PathBuf::new();
    for _ in common..from_components.len() {
        relative.push("..");
    }
    for component in &to_components[common..] {
        relative.push(component.as_os_str());
    }
    if relative.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        relative
    }
}

fn normalize_components(path: &Path) -> Vec<Component<'_>> {
    path.components()
        .filter(|component| !matches!(component, Component::CurDir))
        .collect()
}

fn read_dir_resolved(path: &Path) -> Result<Vec<PathBuf>, String> {
    let root = resolved_existing_dir(path).unwrap_or_else(|| path.to_path_buf());
    let entries =
        fs::read_dir(&root).map_err(|e| format!("Failed to read '{}': {e}", root.display()))?;
    Ok(entries.flatten().map(|entry| entry.path()).collect())
}

fn resolved_existing_dir(path: &Path) -> Option<PathBuf> {
    let meta = fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() {
        fs::canonicalize(path).ok()
    } else {
        Some(path.to_path_buf())
    }
}

fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn copy_dir_entries_resolving_symlinks(src: &Path, dest_dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dest_dir)
        .map_err(|e| format!("Failed to create '{}': {e}", dest_dir.display()))?;
    for entry in read_dir_resolved(src)? {
        let Some(name) = entry.file_name() else {
            continue;
        };
        copy_path_resolving_symlinks(&entry, &dest_dir.join(name))?;
    }
    Ok(())
}

fn copy_path_resolving_symlinks(src: &Path, dest: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(src)
        .map_err(|e| format!("Failed to stat '{}': {e}", src.display()))?;
    if meta.file_type().is_symlink() {
        let resolved = fs::canonicalize(src)
            .map_err(|e| format!("Failed to resolve symlink '{}': {e}", src.display()))?;
        return copy_path_resolving_symlinks(&resolved, dest);
    }

    if meta.is_dir() {
        fs::create_dir_all(dest)
            .map_err(|e| format!("Failed to create '{}': {e}", dest.display()))?;
        let entries =
            fs::read_dir(src).map_err(|e| format!("Failed to read '{}': {e}", src.display()))?;
        for entry in entries.flatten() {
            let child = entry.path();
            let Some(name) = child.file_name() else {
                continue;
            };
            copy_path_resolving_symlinks(&child, &dest.join(name))?;
        }
        return Ok(());
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create '{}': {e}", parent.display()))?;
    }
    fs::copy(src, dest).map_err(|e| {
        format!(
            "Failed to copy '{}' -> '{}': {e}",
            src.display(),
            dest.display()
        )
    })?;
    Ok(())
}

fn copy_skill_dir_resolving_symlinks(src: &Path, dest: &Path) -> Result<(), String> {
    copy_skill_path_resolving_symlinks(src, dest)
}

fn copy_skill_path_resolving_symlinks(src: &Path, dest: &Path) -> Result<(), String> {
    if should_skip_skill_entry(src) {
        return Ok(());
    }

    let meta = fs::symlink_metadata(src)
        .map_err(|e| format!("Failed to stat '{}': {e}", src.display()))?;
    if meta.file_type().is_symlink() {
        let resolved = match fs::canonicalize(src) {
            Ok(resolved) => resolved,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(format!(
                    "Failed to resolve symlink '{}': {error}",
                    src.display()
                ));
            }
        };
        return copy_skill_path_resolving_symlinks(&resolved, dest);
    }

    if meta.is_dir() {
        fs::create_dir_all(dest)
            .map_err(|e| format!("Failed to create '{}': {e}", dest.display()))?;
        let entries =
            fs::read_dir(src).map_err(|e| format!("Failed to read '{}': {e}", src.display()))?;
        for entry in entries.flatten() {
            let child = entry.path();
            let Some(name) = child.file_name() else {
                continue;
            };
            copy_skill_path_resolving_symlinks(&child, &dest.join(name))?;
        }
        return Ok(());
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create '{}': {e}", parent.display()))?;
    }
    fs::copy(src, dest).map_err(|e| {
        format!(
            "Failed to copy '{}' -> '{}': {e}",
            src.display(),
            dest.display()
        )
    })?;
    Ok(())
}

fn should_skip_skill_entry(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            name.starts_with('.')
                || matches!(name, "__pycache__" | "node_modules" | "target" | "venv")
        })
        .unwrap_or(false)
}

fn remove_legacy_path(path: &Path) -> Result<(), String> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return Ok(()),
    };

    if meta.file_type().is_symlink() {
        return remove_link_or_path(path);
    }

    if meta.is_dir() {
        fs::remove_dir_all(path).map_err(|e| format!("Failed to remove '{}': {e}", path.display()))
    } else {
        fs::remove_file(path).map_err(|e| format!("Failed to remove '{}': {e}", path.display()))
    }
}

fn remove_link_or_path(path: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(path)
        .map_err(|e| format!("Failed to stat '{}': {e}", path.display()))?;
    if meta.is_dir() && !meta.file_type().is_symlink() {
        fs::remove_dir_all(path).map_err(|e| format!("Failed to remove '{}': {e}", path.display()))
    } else {
        #[cfg(windows)]
        {
            match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(file_err) if meta.file_type().is_symlink() => {
                    fs::remove_dir(path).map_err(|dir_err| {
                        format!(
                            "Failed to remove '{}': {file_err}; fallback remove_dir also failed: {dir_err}",
                            path.display()
                        )
                    })
                }
                Err(file_err) => {
                    Err(format!("Failed to remove '{}': {file_err}", path.display()))
                }
            }
        }
        #[cfg(not(windows))]
        {
            fs::remove_file(path).map_err(|e| format!("Failed to remove '{}': {e}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_json(path: &Path, value: serde_json::Value) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
    }

    fn write_text(path: &Path, value: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, value).unwrap();
    }

    struct TestHomeGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous_runtime_root: Option<std::ffi::OsString>,
    }

    impl TestHomeGuard {
        fn install(home: &Path, runtime_root: &Path) -> Self {
            let lock = crate::services::discord::runtime_store::test_env_lock()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            fs::create_dir_all(home).unwrap();
            set_test_home_dir_override(Some(home.to_path_buf()));
            let previous_runtime_root = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", runtime_root) };
            Self {
                _lock: lock,
                previous_runtime_root,
            }
        }
    }

    impl Drop for TestHomeGuard {
        fn drop(&mut self) {
            clear_test_home_dir_override();
            match self.previous_runtime_root.as_ref() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    #[test]
    fn ensure_runtime_layout_migrates_legacy_memory_tree() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let home = temp.path().join("home");
        let _home_guard = TestHomeGuard::install(&home, root);

        fs::create_dir_all(root.join("role-context").join("alpha.memory")).unwrap();
        fs::write(
            root.join("role-context")
                .join("alpha.memory")
                .join("notes.md"),
            "# note",
        )
        .unwrap();
        fs::create_dir_all(root.join("shared_agent_memory").join("archive")).unwrap();
        fs::write(
            root.join("shared_agent_memory").join("shared_knowledge.md"),
            "shared",
        )
        .unwrap();
        fs::write(root.join("shared_agent_memory").join("alpha.json"), "{}").unwrap();
        fs::write(
            root.join("agentdesk.yaml"),
            "server:\n  port: 9001\nagents: []\n",
        )
        .unwrap();
        write_json(
            &root.join("config").join("role_map.json"),
            serde_json::json!({
                "version": 1,
                "byChannelId": {
                    "1": {
                        "roleId": "alpha",
                        "promptFile": "/tmp/config/role-context/alpha/IDENTITY.md"
                    }
                }
            }),
        );
        write_json(
            &root.join("config").join("memory-backend.json"),
            serde_json::json!({
                "version": 1,
                "sak_path": "../shared_agent_memory/shared_knowledge.md",
                "sam_path": "../shared_agent_memory",
                "ltm_root": "../role-context"
            }),
        );

        let report = ensure_runtime_layout(root).unwrap();
        assert!(report.migrated);
        assert!(report.backup_path.unwrap().exists());
        assert!(config_file_path(root).is_file());
        assert!(
            shared_agent_knowledge_path(root).is_file(),
            "shared knowledge should move into config/memories"
        );
        assert!(
            shared_agent_memory_root(root).join("alpha.json").is_file(),
            "per-agent SAM should move into config/memories"
        );
        assert!(
            long_term_memory_root(root)
                .join("alpha")
                .join("notes.md")
                .is_file(),
            "role-context/*.memory should move into long-term/<role>"
        );
        assert!(
            !managed_agents_root(root).join("alpha.memory").exists(),
            "role-context/*.memory should not be copied into config/agents"
        );
        let role_map = fs::read_to_string(role_map_path(root)).unwrap();
        assert!(role_map.contains("/tmp/config/agents/alpha/IDENTITY.md"));
        let backend = load_memory_backend(root);
        assert_eq!(backend.version, 2);
        assert_eq!(backend.backend, "auto");
        assert_eq!(backend.file.sak_path, default_sak_path());
        assert_eq!(backend.file.sam_path, default_sam_path());
        assert_eq!(backend.file.ltm_root, default_ltm_root());
        assert_eq!(
            backend.file.auto_memory_root,
            "~/.claude/projects/*{workspace}*/memory/"
        );
        assert!(!root.join("shared_agent_memory").exists());
        assert!(!root.join("role-context").exists());
        assert!(!root.join("agentdesk.yaml").exists());
    }

    #[test]
    fn sync_managed_skills_deploys_relative_links() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let home = temp.path().join("home");
        let _home_guard = TestHomeGuard::install(&home, root);

        fs::create_dir_all(root.join("workspaces").join("alpha")).unwrap();
        fs::create_dir_all(root.join("skills").join("memory-read")).unwrap();
        fs::write(
            root.join("skills").join("memory-read").join("SKILL.md"),
            "# memory-read\nbody",
        )
        .unwrap();
        ensure_managed_skills_manifest(root).unwrap();

        let report = sync_managed_skills(root).unwrap();
        assert!(report.created_links >= 2);
        let codex_link = home.join(".codex").join("skills").join("memory-read");
        let claude_link = home.join(".claude").join("commands").join("memory-read.md");
        assert!(
            fs::symlink_metadata(&codex_link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            fs::symlink_metadata(&claude_link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn remove_legacy_path_unlinks_symlink_without_deleting_source() {
        let temp = tempfile::tempdir().unwrap();
        let source_dir = temp.path().join("obsidian").join("role-context");
        let link_path = temp.path().join("runtime-role-context");
        write_text(&source_dir.join("alpha").join("IDENTITY.md"), "# alpha");
        create_symlink_entry(&source_dir, &link_path, true).unwrap();

        remove_legacy_path(&link_path).unwrap();

        assert!(
            source_dir.join("alpha").join("IDENTITY.md").is_file(),
            "removing a migrated symlink must not delete the original source"
        );
        assert!(
            !path_exists(&link_path),
            "legacy symlink itself should be removed after migration"
        );
    }

    #[test]
    fn ensure_runtime_layout_preserves_custom_v2_memory_backend() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let _home_guard = TestHomeGuard::install(&temp.path().join("home"), root);
        write_json(
            &memory_backend_path(root),
            serde_json::json!({
                "version": 2,
                "backend": "memento",
                "file": {
                    "sak_path": "/tmp/custom/shared.md",
                    "sam_path": "/tmp/custom/sam",
                    "ltm_root": "/tmp/custom/ltm",
                    "auto_memory_root": "/tmp/custom/auto/{workspace}"
                },
                "mcp": {
                    "endpoint": "http://127.0.0.1:8765",
                    "access_key_env": "MEMENTO_API_KEY"
                }
            }),
        );

        let report = ensure_runtime_layout(root).unwrap();
        let backend = load_memory_backend(root);

        assert!(!report.migrated);
        assert_eq!(backend.version, 2);
        assert_eq!(backend.backend, "memento");
        assert_eq!(backend.file.sak_path, "/tmp/custom/shared.md");
        assert_eq!(backend.file.sam_path, "/tmp/custom/sam");
        assert_eq!(backend.file.ltm_root, "/tmp/custom/ltm");
        assert_eq!(
            backend.file.auto_memory_root,
            "/tmp/custom/auto/{workspace}"
        );
        assert_eq!(backend.mcp.endpoint, "http://127.0.0.1:8765");
        assert_eq!(backend.mcp.access_key_env, "MEMENTO_API_KEY");
    }

    #[test]
    fn load_memory_backend_preserves_legacy_v1_paths_until_migration() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let _home_guard = TestHomeGuard::install(&temp.path().join("home"), root);
        write_json(
            &root.join("memory-backend.json"),
            serde_json::json!({
                "version": 1,
                "sak_path": "/tmp/legacy/shared.md",
                "sam_path": "/tmp/legacy/sam",
                "ltm_root": "/tmp/legacy/ltm"
            }),
        );

        let backend = load_memory_backend(root);

        assert_eq!(backend.version, 1);
        assert_eq!(backend.backend, "auto");
        assert_eq!(backend.file.sak_path, "/tmp/legacy/shared.md");
        assert_eq!(backend.file.sam_path, "/tmp/legacy/sam");
        assert_eq!(backend.file.ltm_root, "/tmp/legacy/ltm");
        assert_eq!(
            backend.file.auto_memory_root,
            "~/.claude/projects/*{workspace}*/memory/"
        );
    }

    #[test]
    fn resolve_memory_path_expands_tilde_paths() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let root = temp.path().join("runtime");
        let _home_guard = TestHomeGuard::install(&home, &root);

        let resolved = resolve_memory_path(&root, "~/custom/shared.md");

        assert_eq!(resolved, home.join("custom").join("shared.md"));
    }

    #[test]
    fn sync_managed_skills_supports_legacy_manifest_targets() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let home = temp.path().join("home");
        let workspace = home.join("workspace-alpha");
        let _home_guard = TestHomeGuard::install(&home, root);

        fs::create_dir_all(&workspace).unwrap();
        write_text(
            &root.join("skills").join("memory-read").join("SKILL.md"),
            "# memory-read\nbody",
        );
        write_json(
            &managed_skills_manifest_path(root),
            serde_json::json!({
                "memory-read": {
                    "targets": ["claude", "codex", "claude@~/workspace-alpha", "codex@~/workspace-alpha"]
                }
            }),
        );

        let report = sync_managed_skills(root).unwrap();

        assert!(report.created_links >= 4);
        assert!(
            home.join(".claude")
                .join("commands")
                .join("memory-read.md")
                .exists()
        );
        assert!(
            home.join(".codex")
                .join("skills")
                .join("memory-read")
                .exists()
        );
        assert!(
            workspace
                .join(".claude")
                .join("commands")
                .join("memory-read.md")
                .exists()
        );
        assert!(
            workspace
                .join(".codex")
                .join("skills")
                .join("memory-read")
                .exists()
        );
    }

    #[test]
    fn ensure_runtime_layout_migrates_legacy_skill_links_and_rewrites_memory_paths() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let home = temp.path().join("home");
        let _home_guard = TestHomeGuard::install(&home, root);

        let legacy_skill_dir = temp.path().join("obsidian").join("memory-write");
        write_text(
            &legacy_skill_dir.join("SKILL.md"),
            "Use ~/.adk/release/shared_agent_memory/shared_knowledge.md and ~/.adk/release/shared_agent_memory/{role_id}.json and ~/.claude/projects/-Users-itismyfield--adk-release-workspaces-{workspace}/memory/",
        );
        let legacy_codex_link = home.join(".codex").join("skills").join("memory-write");
        fs::create_dir_all(legacy_codex_link.parent().unwrap()).unwrap();
        create_symlink_entry(&legacy_skill_dir, &legacy_codex_link, true).unwrap();

        ensure_runtime_layout(root).unwrap();

        let managed_skill = managed_skills_root(root)
            .join("memory-write")
            .join("SKILL.md");
        let managed_content = fs::read_to_string(&managed_skill).unwrap();
        let original_content = fs::read_to_string(legacy_skill_dir.join("SKILL.md")).unwrap();

        assert!(managed_skill.is_file());
        assert!(
            managed_content.contains("config/memories/shared-agent-knowledge/shared_knowledge.md")
        );
        assert!(managed_content.contains("config/memories/shared-agent-memory/{role_id}.json"));
        assert!(managed_content.contains("~/.claude/projects/*{workspace}*/memory/"));
        assert!(original_content.contains("shared_agent_memory/shared_knowledge.md"));
    }

    #[cfg(unix)]
    #[test]
    fn ensure_managed_skill_dir_skips_hidden_artifacts() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("runtime");
        let source_skill_dir = temp.path().join("source-skill");
        let hidden_dir = source_skill_dir.join("references").join(".venv");
        write_text(&source_skill_dir.join("SKILL.md"), "# hidden-test");
        fs::create_dir_all(&hidden_dir).unwrap();
        symlink(
            hidden_dir.join("missing.txt"),
            hidden_dir.join("broken-link.txt"),
        )
        .unwrap();

        let managed = ensure_managed_skill_dir(&root, "hidden-test", &source_skill_dir).unwrap();

        assert!(managed.join("SKILL.md").is_file());
        assert!(!managed.join("references").join(".venv").exists());
    }
}
