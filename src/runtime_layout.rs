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

pub fn shared_prompt_path(root: &Path) -> PathBuf {
    managed_agents_root(root).join("_shared.prompt.md")
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
    if let Some(config) = load_memory_backend_from_yaml(root) {
        return config.with_defaults();
    }

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
    normalize_agent_config_channels(root)?;
    synchronize_shared_prompt(root)?;
    update_role_map_prompt_paths(root)?;
    merge_role_map_into_agentdesk_yaml(root)?;
    update_org_yaml_prompt_paths(root)?;
    ensure_managed_skills_manifest(root)?;
    migrate_legacy_skill_links(root)?;
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
    Ok(())
}

fn normalize_agent_config_channels(root: &Path) -> Result<(), String> {
    let path = config_file_path(root);
    if !path.is_file() {
        return Ok(());
    }

    let content = fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
    let normalized = strip_dead_agent_channel_token_lines(&content);
    if normalized != content {
        fs::write(&path, normalized)
            .map_err(|e| format!("Failed to write '{}': {e}", path.display()))?;
    }
    Ok(())
}

fn strip_dead_agent_channel_token_lines(content: &str) -> String {
    let mut output = Vec::new();
    let mut in_agents = false;
    let mut in_agent_channels = false;

    for line in content.lines() {
        let indent = line.chars().take_while(|ch| *ch == ' ').count();
        let trimmed = line.trim_start();

        if indent == 0 && trimmed.starts_with("agents:") {
            in_agents = true;
            in_agent_channels = false;
            output.push(line);
            continue;
        }
        if indent == 0 && !trimmed.is_empty() && !trimmed.starts_with('#') {
            in_agents = false;
            in_agent_channels = false;
        }
        if in_agents && indent == 4 && trimmed.starts_with("channels:") {
            in_agent_channels = true;
            output.push(line);
            continue;
        }
        if in_agent_channels && indent <= 4 && !trimmed.is_empty() {
            in_agent_channels = false;
        }
        if in_agent_channels && indent >= 6 && trimmed.starts_with("token:") {
            continue;
        }
        output.push(line);
    }

    let mut rendered = output.join("\n");
    if content.ends_with('\n') {
        rendered.push('\n');
    }
    rendered
}

fn synchronize_shared_prompt(root: &Path) -> Result<(), String> {
    let canonical = shared_prompt_path(root);
    let aliases = shared_prompt_aliases(root);
    let source = std::iter::once(canonical.clone())
        .chain(aliases.iter().cloned())
        .find(|path| path.is_file());

    let Some(source_path) = source else {
        return Ok(());
    };

    if canonical != source_path {
        if let Some(parent) = canonical.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create '{}': {e}", parent.display()))?;
        }
        copy_path_resolving_symlinks(&source_path, &canonical)?;
    }

    for alias in aliases {
        if same_canonical_path(&alias, &canonical) {
            continue;
        }
        if let Some(parent) = alias.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create '{}': {e}", parent.display()))?;
        }
        if path_exists(&alias) {
            remove_link_or_path(&alias)?;
        }
        create_symlink_entry(&canonical, &alias, false)?;
    }

    Ok(())
}

fn shared_prompt_aliases(root: &Path) -> Vec<PathBuf> {
    let mut aliases = vec![
        config_dir(root).join("_shared.md"),
        managed_agents_root(root).join("_shared.md"),
        config_dir(root)
            .join("role-context")
            .join("_shared.prompt.md"),
        root.join("role-context").join("_shared.prompt.md"),
    ];

    if let Some(home) =
        current_home_dir().filter(|home| manages_home_shared_prompt_aliases(root, home))
    {
        aliases.push(home.join(".agentdesk").join("prompts").join("_shared.md"));
        aliases.push(
            home.join(".agentdesk")
                .join("role-context")
                .join("_shared.prompt.md"),
        );
    }

    aliases.sort();
    aliases.dedup();
    aliases
}

fn manages_home_shared_prompt_aliases(root: &Path, home: &Path) -> bool {
    let release_root = home.join(".adk").join("release");
    same_canonical_path(root, &release_root)
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
    rewrite_legacy_managed_memory_paths(root, &mut backend);

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

fn rewrite_legacy_managed_memory_paths(root: &Path, backend: &mut MemoryBackendConfig) {
    backend.file.sak_path = rewrite_legacy_managed_memory_path(
        root,
        &backend.file.sak_path,
        &[
            root.join("shared_agent_memory").join("shared_knowledge.md"),
            config_dir(root)
                .join("shared_agent_memory")
                .join("shared_knowledge.md"),
        ],
        default_sak_path,
    );
    backend.file.sam_path = rewrite_legacy_managed_memory_path(
        root,
        &backend.file.sam_path,
        &[
            root.join("shared_agent_memory"),
            config_dir(root).join("shared_agent_memory"),
        ],
        default_sam_path,
    );
    backend.file.ltm_root = rewrite_legacy_managed_memory_path(
        root,
        &backend.file.ltm_root,
        &[
            root.join("role-context"),
            config_dir(root).join("role-context"),
            root.join("long-term-memory"),
            config_dir(root).join("long-term-memory"),
        ],
        default_ltm_root,
    );
}

fn load_memory_backend_from_yaml(root: &Path) -> Option<MemoryBackendConfig> {
    for path in [config_file_path(root), legacy_config_file_path(root)] {
        if !path.is_file() {
            continue;
        }

        match crate::config::load_from_path(&path) {
            Ok(config) => {
                if let Some(memory) = config.memory {
                    return Some(memory_backend_from_config(memory));
                }
            }
            Err(error) => {
                eprintln!(
                    "  [memory] Warning: failed to parse '{}' for memory config: {error}",
                    path.display()
                );
            }
        }
    }

    None
}

fn memory_backend_from_config(config: crate::config::MemoryConfig) -> MemoryBackendConfig {
    MemoryBackendConfig {
        version: MEMORY_LAYOUT_VERSION,
        backend: config.backend,
        file: FileMemoryBackendConfig {
            sak_path: config.file.sak_path,
            sam_path: config.file.sam_path,
            ltm_root: config.file.ltm_root,
            auto_memory_root: config.file.auto_memory_root,
        },
        mcp: McpMemoryBackendConfig {
            endpoint: config.mcp.endpoint,
            access_key_env: config.mcp.access_key_env,
        },
        legacy_sak_path: None,
        legacy_sam_path: None,
        legacy_ltm_root: None,
    }
}

fn rewrite_legacy_managed_memory_path(
    root: &Path,
    raw: &str,
    legacy_candidates: &[PathBuf],
    replacement: fn() -> String,
) -> String {
    let resolved = resolve_memory_path(root, raw);
    if legacy_candidates
        .iter()
        .any(|candidate| same_canonical_path(&resolved, candidate))
    {
        return replacement();
    }
    raw.to_string()
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
    rewrite_prompt_paths_json(&mut json, root);
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
    rewrite_prompt_paths_yaml(&mut yaml, root);
    let rendered = serde_yaml::to_string(&yaml)
        .map_err(|e| format!("Failed to serialize '{}': {e}", path.display()))?;
    fs::write(&path, rendered).map_err(|e| format!("Failed to write '{}': {e}", path.display()))
}

#[derive(Debug, Default)]
struct AgentChannelUpdate {
    id: Option<String>,
    name: Option<String>,
    prompt_file: Option<String>,
    workspace: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
    peer_agents: Option<bool>,
}

fn merge_role_map_into_agentdesk_yaml(root: &Path) -> Result<(), String> {
    let role_map = role_map_path(root);
    if !role_map.is_file() {
        return Ok(());
    }

    let yaml_path = config_file_path(root);
    let mut config = if yaml_path.is_file() {
        crate::config::load_from_path(&yaml_path)
            .map_err(|e| format!("Failed to load config '{}': {e}", yaml_path.display()))?
    } else {
        crate::config::Config::default()
    };

    let content = fs::read_to_string(&role_map)
        .map_err(|e| format!("Failed to read '{}': {e}", role_map.display()))?;
    let json: Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse '{}': {e}", role_map.display()))?;

    let mut changed = false;
    changed |= merge_role_map_shared_prompt(&mut config, &json);
    changed |= merge_role_map_meeting(&mut config, &json);

    let mut providers_by_channel_id = BTreeMap::<String, String>::new();
    if let Some(by_id) = json.get("byChannelId").and_then(Value::as_object) {
        for (channel_id, entry) in by_id {
            if let Some((provider_key, entry_changed)) =
                merge_role_map_channel_id_entry(&mut config, channel_id, entry)
            {
                providers_by_channel_id.insert(channel_id.clone(), provider_key);
                changed |= entry_changed;
            }
        }
    }
    if let Some(by_name) = json.get("byChannelName").and_then(Value::as_object) {
        for (channel_name, entry) in by_name {
            if merge_role_map_channel_name_entry(
                &mut config,
                channel_name,
                entry,
                &providers_by_channel_id,
            ) {
                changed = true;
            }
        }
    }

    if changed {
        crate::config::save_to_path(&yaml_path, &config)
            .map_err(|e| format!("Failed to write config '{}': {e}", yaml_path.display()))?;
    }
    Ok(())
}

fn merge_role_map_shared_prompt(config: &mut crate::config::Config, json: &Value) -> bool {
    if config.shared_prompt.is_some() {
        return false;
    }
    let Some(shared_prompt) = json_string_field(json, &["sharedPromptFile", "shared_prompt"])
    else {
        return false;
    };
    config.shared_prompt = Some(shared_prompt);
    true
}

fn merge_role_map_meeting(config: &mut crate::config::Config, json: &Value) -> bool {
    if config.meeting.is_some() {
        return false;
    }
    let Some(meeting) = json.get("meeting").and_then(role_map_meeting_to_config) else {
        return false;
    };
    config.meeting = Some(meeting);
    true
}

fn role_map_meeting_to_config(value: &Value) -> Option<crate::config::MeetingSettings> {
    let meeting = value.as_object()?;
    let channel_name = json_string_field_from_map(meeting, &["channel_name"])?;
    let max_rounds = meeting
        .get("max_rounds")
        .and_then(Value::as_u64)
        .map(|value| value as u32);
    let max_participants = meeting
        .get("max_participants")
        .or_else(|| meeting.get("maxParticipants"))
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    let summary_agent = meeting
        .get("summary_agent")
        .and_then(role_map_summary_agent_to_config);
    let available_agents = meeting
        .get("available_agents")
        .and_then(Value::as_array)
        .map(|agents| {
            agents
                .iter()
                .filter_map(role_map_meeting_agent_to_config)
                .collect::<Vec<_>>()
        });

    Some(crate::config::MeetingSettings {
        channel_name,
        max_rounds,
        max_participants,
        summary_agent,
        available_agents,
    })
}

fn role_map_summary_agent_to_config(
    value: &Value,
) -> Option<crate::config::MeetingSummaryAgentDef> {
    if let Some(agent) = value.as_str().and_then(|raw| normalize_non_empty(raw)) {
        return Some(crate::config::MeetingSummaryAgentDef::Static(agent));
    }

    let obj = value.as_object()?;
    let default = json_string_field_from_map(obj, &["default"])?;
    let rules = obj
        .get("rules")
        .and_then(Value::as_array)
        .map(|rules| {
            rules
                .iter()
                .filter_map(|rule| {
                    let rule_obj = rule.as_object()?;
                    let agent = json_string_field_from_map(rule_obj, &["agent"])?;
                    let keywords = rule_obj
                        .get("keywords")
                        .and_then(Value::as_array)
                        .map(|keywords| {
                            keywords
                                .iter()
                                .filter_map(Value::as_str)
                                .filter_map(normalize_non_empty)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    Some(crate::config::MeetingSummaryRuleDef { keywords, agent })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Some(crate::config::MeetingSummaryAgentDef::Dynamic { rules, default })
}

fn role_map_meeting_agent_to_config(value: &Value) -> Option<crate::config::MeetingAgentEntry> {
    let obj = value.as_object()?;
    let role_id = json_string_field_from_map(obj, &["role_id", "roleId"])?;
    let display_name = json_string_field_from_map(obj, &["display_name", "displayName"]);
    let prompt_file = json_string_field_from_map(obj, &["prompt_file", "promptFile"]);
    let domain_summary = json_string_field_from_map(obj, &["domain_summary", "domainSummary"]);
    let provider_hint =
        json_string_field_from_map(obj, &["provider_hint", "providerHint", "provider"]);
    let keywords = obj
        .get("keywords")
        .and_then(Value::as_array)
        .map(|keywords| {
            keywords
                .iter()
                .filter_map(Value::as_str)
                .filter_map(normalize_non_empty)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let strengths = json_string_vec_field_from_map(obj, &["strengths"]);
    let task_types = json_string_vec_field_from_map(obj, &["task_types", "taskTypes"]);
    let anti_signals = json_string_vec_field_from_map(obj, &["anti_signals", "antiSignals"]);

    Some(crate::config::MeetingAgentEntry::Detailed(
        crate::config::MeetingAgentDef {
            role_id,
            display_name,
            keywords,
            prompt_file,
            domain_summary,
            strengths,
            task_types,
            anti_signals,
            provider_hint,
        },
    ))
}

fn merge_role_map_channel_id_entry(
    config: &mut crate::config::Config,
    channel_id: &str,
    entry: &Value,
) -> Option<(String, bool)> {
    let obj = entry.as_object()?;
    let role_id = json_string_field_from_map(obj, &["roleId", "role_id"])?;
    let provider_key = json_string_field_from_map(obj, &["provider"])
        .as_deref()
        .and_then(normalize_provider_name)
        .or_else(|| infer_provider_for_role(config, &role_id, Some(channel_id), None))
        .unwrap_or_else(|| "claude".to_string());

    let (agent_index, agent_changed) = ensure_config_agent(config, &role_id, &provider_key);
    let update = AgentChannelUpdate {
        id: normalize_non_empty(channel_id),
        name: json_string_field_from_map(obj, &["channelName", "channel_name"]),
        prompt_file: json_string_field_from_map(obj, &["promptFile", "prompt_file"]),
        workspace: json_string_field_from_map(obj, &["workspace"]),
        provider: Some(provider_key.clone()),
        model: json_string_field_from_map(obj, &["model"]),
        reasoning_effort: json_string_field_from_map(obj, &["reasoningEffort", "reasoning_effort"]),
        peer_agents: json_bool_field_from_map(obj, &["peerAgents", "peer_agents"]),
    };

    let agent = &mut config.agents[agent_index];
    let slot = channel_slot_mut(&mut agent.channels, &provider_key)?;
    let channel_changed = apply_channel_update(slot, update, None);
    Some((provider_key, agent_changed || channel_changed))
}

fn merge_role_map_channel_name_entry(
    config: &mut crate::config::Config,
    channel_name: &str,
    entry: &Value,
    providers_by_channel_id: &BTreeMap<String, String>,
) -> bool {
    let Some(obj) = entry.as_object() else {
        return false;
    };
    let Some(role_id) = json_string_field_from_map(obj, &["roleId", "role_id"]) else {
        return false;
    };
    let channel_id = json_string_field_from_map(obj, &["channelId", "channel_id"]);
    let provider_key = json_string_field_from_map(obj, &["provider"])
        .as_deref()
        .and_then(normalize_provider_name)
        .or_else(|| {
            channel_id
                .as_ref()
                .and_then(|channel_id| providers_by_channel_id.get(channel_id).cloned())
        })
        .or_else(|| {
            infer_provider_for_role(config, &role_id, channel_id.as_deref(), Some(channel_name))
        })
        .unwrap_or_else(|| "claude".to_string());

    let (agent_index, agent_changed) = ensure_config_agent(config, &role_id, &provider_key);
    let update = AgentChannelUpdate {
        id: channel_id,
        name: normalize_non_empty(channel_name),
        prompt_file: json_string_field_from_map(obj, &["promptFile", "prompt_file"]),
        workspace: json_string_field_from_map(obj, &["workspace"]),
        provider: Some(provider_key.clone()),
        model: json_string_field_from_map(obj, &["model"]),
        reasoning_effort: json_string_field_from_map(obj, &["reasoningEffort", "reasoning_effort"]),
        peer_agents: json_bool_field_from_map(obj, &["peerAgents", "peer_agents"]),
    };

    let agent = &mut config.agents[agent_index];
    let Some(slot) = channel_slot_mut(&mut agent.channels, &provider_key) else {
        return agent_changed;
    };
    agent_changed || apply_channel_update(slot, update, None)
}

fn ensure_config_agent(
    config: &mut crate::config::Config,
    role_id: &str,
    provider_key: &str,
) -> (usize, bool) {
    if let Some(index) = config.agents.iter().position(|agent| agent.id == role_id) {
        let agent = &mut config.agents[index];
        if normalize_provider_name(&agent.provider).is_none() {
            agent.provider = provider_key.to_string();
            return (index, true);
        }
        return (index, false);
    }

    config.agents.push(crate::config::AgentDef {
        id: role_id.to_string(),
        name: role_id.to_string(),
        name_ko: None,
        provider: provider_key.to_string(),
        channels: crate::config::AgentChannels::default(),
        keywords: Vec::new(),
        department: None,
        avatar_emoji: None,
    });
    (config.agents.len() - 1, true)
}

fn infer_provider_for_role(
    config: &crate::config::Config,
    role_id: &str,
    channel_id: Option<&str>,
    channel_name: Option<&str>,
) -> Option<String> {
    let agent = config.agents.iter().find(|agent| agent.id == role_id)?;
    for (provider_key, maybe_channel) in agent.channels.iter() {
        let Some(channel) = maybe_channel else {
            continue;
        };
        if let Some(channel_id) = channel_id
            && (channel.channel_id().as_deref() == Some(channel_id)
                || channel.target().as_deref() == Some(channel_id))
        {
            return Some(provider_key.to_string());
        }
        if let Some(channel_name) = channel_name
            && (channel.channel_name().as_deref() == Some(channel_name)
                || channel.aliases().iter().any(|alias| alias == channel_name))
        {
            return Some(provider_key.to_string());
        }
    }
    normalize_provider_name(&agent.provider)
}

fn channel_slot_mut<'a>(
    channels: &'a mut crate::config::AgentChannels,
    provider: &str,
) -> Option<&'a mut Option<crate::config::AgentChannel>> {
    match provider {
        "claude" => Some(&mut channels.claude),
        "codex" => Some(&mut channels.codex),
        "gemini" => Some(&mut channels.gemini),
        "qwen" => Some(&mut channels.qwen),
        _ => None,
    }
}

fn apply_channel_update(
    slot: &mut Option<crate::config::AgentChannel>,
    update: AgentChannelUpdate,
    extra_aliases: Option<Vec<String>>,
) -> bool {
    let current = slot.clone();
    let mut config = match current.clone() {
        Some(crate::config::AgentChannel::Detailed(config)) => config,
        Some(crate::config::AgentChannel::Legacy(raw)) => channel_config_from_legacy(raw),
        None => crate::config::AgentChannelConfig::default(),
    };

    if config.id.is_none() {
        config.id = update.id;
    }
    if let Some(name) = update.name {
        match config.name.as_deref() {
            Some(existing) if existing == name => {}
            Some(_) => push_channel_alias(&mut config, name),
            None => config.name = Some(name),
        }
    }
    if config.prompt_file.is_none() {
        config.prompt_file = update.prompt_file;
    }
    if config.workspace.is_none() {
        config.workspace = update.workspace;
    }
    if config.provider.is_none() {
        config.provider = update.provider;
    }
    if config.model.is_none() {
        config.model = update.model;
    }
    if config.reasoning_effort.is_none() {
        config.reasoning_effort = update.reasoning_effort;
    }
    if config.peer_agents.is_none() {
        config.peer_agents = update.peer_agents;
    }
    if let Some(extra_aliases) = extra_aliases {
        for alias in extra_aliases {
            push_channel_alias(&mut config, alias);
        }
    }

    let next = Some(crate::config::AgentChannel::Detailed(config));
    if next != current {
        *slot = next;
        true
    } else {
        false
    }
}

fn channel_config_from_legacy(raw: String) -> crate::config::AgentChannelConfig {
    let mut config = crate::config::AgentChannelConfig::default();
    let Some(raw) = normalize_non_empty(&raw) else {
        return config;
    };
    if raw.parse::<u64>().is_ok() {
        config.id = Some(raw);
    } else {
        config.name = Some(raw);
    }
    config
}

fn push_channel_alias(config: &mut crate::config::AgentChannelConfig, alias: String) {
    let Some(alias) = normalize_non_empty(&alias) else {
        return;
    };
    if config.name.as_deref() == Some(alias.as_str()) {
        return;
    }
    if !config.aliases.iter().any(|existing| existing == &alias) {
        config.aliases.push(alias);
        config.aliases.sort();
        config.aliases.dedup();
    }
}

fn normalize_non_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn json_string_field(value: &Value, keys: &[&str]) -> Option<String> {
    let obj = value.as_object()?;
    json_string_field_from_map(obj, keys)
}

fn json_string_field_from_map(
    obj: &serde_json::Map<String, Value>,
    keys: &[&str],
) -> Option<String> {
    keys.iter().find_map(|key| {
        obj.get(*key).and_then(|value| match value {
            Value::String(raw) => normalize_non_empty(raw),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        })
    })
}

fn json_string_vec_field_from_map(
    obj: &serde_json::Map<String, Value>,
    keys: &[&str],
) -> Vec<String> {
    keys.iter()
        .find_map(|key| obj.get(*key).and_then(Value::as_array))
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .filter_map(normalize_non_empty)
                .collect()
        })
        .unwrap_or_default()
}

fn json_bool_field_from_map(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| obj.get(*key).and_then(Value::as_bool))
}

fn rewrite_prompt_paths_json(value: &mut Value, root: &Path) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if matches!(key.as_str(), "promptFile" | "prompt_file") {
                    if let Some(raw) = child.as_str() {
                        *child = Value::String(rewrite_prompt_path(raw));
                    }
                } else if key == "sharedPromptFile" {
                    *child = Value::String(shared_prompt_path(root).display().to_string());
                } else {
                    rewrite_prompt_paths_json(child, root);
                }
            }
        }
        Value::Array(items) => {
            for child in items {
                rewrite_prompt_paths_json(child, root);
            }
        }
        _ => {}
    }
}

fn rewrite_prompt_paths_yaml(value: &mut serde_yaml::Value, root: &Path) {
    match value {
        serde_yaml::Value::Mapping(map) => {
            for (key, child) in map.iter_mut() {
                let key_str = key.as_str().unwrap_or_default();
                if matches!(key_str, "promptFile" | "prompt_file") {
                    if let Some(raw) = child.as_str() {
                        *child = serde_yaml::Value::String(rewrite_prompt_path(raw));
                    }
                } else if key_str == "shared_prompt" {
                    *child =
                        serde_yaml::Value::String(shared_prompt_path(root).display().to_string());
                } else {
                    rewrite_prompt_paths_yaml(child, root);
                }
            }
        }
        serde_yaml::Value::Sequence(items) => {
            for child in items {
                rewrite_prompt_paths_yaml(child, root);
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
    if meta.file_type().is_symlink() {
        #[cfg(windows)]
        {
            if fs::metadata(path)
                .map(|target| target.is_dir())
                .unwrap_or(false)
            {
                return fs::remove_dir(path)
                    .map_err(|e| format!("Failed to remove '{}': {e}", path.display()));
            }
        }
        return fs::remove_file(path)
            .map_err(|e| format!("Failed to remove '{}': {e}", path.display()));
    }

    if meta.is_dir() {
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
    fn ensure_runtime_layout_strips_dead_agent_channel_token_lines_and_syncs_shared_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let root = home.join(".adk").join("release");
        let _home_guard = TestHomeGuard::install(&home, &root);

        write_text(
            &config_file_path(&root),
            r#"server:
  port: 9001
agents:
  - id: alpha
    name: Alpha
    provider: claude
    channels:
      claude: "111"
      token: "legacy-claude-token"
      codex: "222"
      token: "legacy-codex-token"
"#,
        );
        write_text(
            &home.join(".agentdesk").join("prompts").join("_shared.md"),
            "# shared prompt",
        );

        ensure_runtime_layout(&root).unwrap();

        let yaml = fs::read_to_string(config_file_path(&root)).unwrap();
        assert!(yaml.contains("claude: \"111\""));
        assert!(yaml.contains("codex: \"222\""));
        assert!(!yaml.contains("token:"));

        let canonical = shared_prompt_path(&root);
        assert_eq!(fs::read_to_string(&canonical).unwrap(), "# shared prompt");
        let legacy_alias = home.join(".agentdesk").join("prompts").join("_shared.md");
        assert!(
            fs::symlink_metadata(&legacy_alias)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(same_canonical_path(&legacy_alias, &canonical));
    }

    #[test]
    fn ensure_runtime_layout_merges_role_map_into_agentdesk_yaml() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();

        write_text(
            &config_file_path(root),
            "server:\n  port: 9001\nagents: []\n",
        );
        write_json(
            &role_map_path(root),
            serde_json::json!({
                "version": 1,
                "sharedPromptFile": "/tmp/legacy/shared.md",
                "byChannelId": {
                    "1479671301387059200": {
                        "roleId": "project-agentdesk",
                        "provider": "codex",
                        "promptFile": "/tmp/config/role-context/project-agentdesk/IDENTITY.md",
                        "workspace": "/tmp/workspaces/agentdesk"
                    }
                },
                "byChannelName": {
                    "adk-cdx": {
                        "roleId": "project-agentdesk",
                        "channelId": "1479671301387059200",
                        "workspace": "/tmp/workspaces/agentdesk"
                    }
                },
                "meeting": {
                    "channel_name": "round-table",
                    "max_rounds": 4,
                    "summary_agent": {
                        "default": "project-agentdesk",
                        "rules": [
                            {
                                "keywords": ["ops"],
                                "agent": "project-agentdesk"
                            }
                        ]
                    },
                    "available_agents": [
                        {
                            "role_id": "project-agentdesk",
                            "display_name": "AgentDesk",
                            "keywords": ["ops"],
                            "prompt_file": "/tmp/config/role-context/project-agentdesk/IDENTITY.md"
                        }
                    ]
                }
            }),
        );

        ensure_runtime_layout(root).unwrap();

        let config = crate::config::load_from_path(&config_file_path(root)).unwrap();
        let shared_prompt = shared_prompt_path(root).display().to_string();
        assert_eq!(
            config.shared_prompt.as_deref(),
            Some(shared_prompt.as_str())
        );

        let meeting = config.meeting.expect("meeting config");
        assert_eq!(meeting.channel_name, "round-table");
        assert_eq!(meeting.max_rounds, Some(4));
        assert_eq!(
            meeting.available_agents.as_ref().map(|agents| agents.len()),
            Some(1)
        );

        let agent = config
            .agents
            .iter()
            .find(|agent| agent.id == "project-agentdesk")
            .expect("migrated agent");
        assert_eq!(agent.provider, "codex");
        let codex_channel = match agent.channels.codex.as_ref().expect("codex channel") {
            crate::config::AgentChannel::Detailed(channel) => channel,
            crate::config::AgentChannel::Legacy(_) => panic!("expected detailed channel config"),
        };
        assert_eq!(codex_channel.id.as_deref(), Some("1479671301387059200"));
        assert_eq!(codex_channel.name.as_deref(), Some("adk-cdx"));
        assert_eq!(
            codex_channel.workspace.as_deref(),
            Some("/tmp/workspaces/agentdesk")
        );
        assert_eq!(
            codex_channel.prompt_file.as_deref(),
            Some("/tmp/config/agents/project-agentdesk/IDENTITY.md")
        );
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
    fn load_memory_backend_prefers_agentdesk_yaml_memory_section() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let _home_guard = TestHomeGuard::install(&temp.path().join("home"), root);
        write_text(
            &config_file_path(root),
            r#"server:
  port: 9001
memory:
  backend: memento
  file:
    sak_path: /tmp/yaml/shared.md
    sam_path: /tmp/yaml/sam
    ltm_root: /tmp/yaml/ltm
    auto_memory_root: /tmp/yaml/auto/{workspace}
  mcp:
    endpoint: http://127.0.0.1:8765
    access_key_env: MEMENTO_API_KEY
"#,
        );
        write_json(
            &memory_backend_path(root),
            serde_json::json!({
                "version": 2,
                "backend": "file",
                "file": {
                    "sak_path": "/tmp/json/shared.md",
                    "sam_path": "/tmp/json/sam",
                    "ltm_root": "/tmp/json/ltm",
                    "auto_memory_root": "/tmp/json/auto/{workspace}"
                }
            }),
        );

        let backend = load_memory_backend(root);

        assert_eq!(backend.version, 2);
        assert_eq!(backend.backend, "memento");
        assert_eq!(backend.file.sak_path, "/tmp/yaml/shared.md");
        assert_eq!(backend.file.sam_path, "/tmp/yaml/sam");
        assert_eq!(backend.file.ltm_root, "/tmp/yaml/ltm");
        assert_eq!(backend.file.auto_memory_root, "/tmp/yaml/auto/{workspace}");
        assert_eq!(backend.mcp.endpoint, "http://127.0.0.1:8765");
        assert_eq!(backend.mcp.access_key_env, "MEMENTO_API_KEY");
    }

    #[test]
    fn ensure_runtime_layout_does_not_materialize_memory_backend_json_without_legacy_input() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let _home_guard = TestHomeGuard::install(&temp.path().join("home"), root);
        write_text(&config_file_path(root), "server:\n  port: 9001\n");

        let report = ensure_runtime_layout(root).unwrap();

        assert!(!report.migrated);
        assert!(!memory_backend_path(root).exists());
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
