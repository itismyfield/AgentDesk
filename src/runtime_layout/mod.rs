mod config_merge;
mod legacy_migration;
mod paths;
mod skill_refresh;
mod skill_sync;

use config_merge::{
    merge_role_map_into_agentdesk_yaml, update_org_yaml_prompt_paths, update_role_map_prompt_paths,
};
use legacy_migration::{
    create_legacy_backup, migrate_legacy_layout, normalize_agent_config_channels,
    synchronize_shared_prompt,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use skill_sync::{ensure_managed_skills_manifest, migrate_legacy_skill_links};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

pub(crate) use config_merge::preview_role_map_merge;
#[allow(unused_imports)]
pub use paths::{
    config_dir, config_file_path, credential_dir, credential_token_path, expand_user_path,
    legacy_config_file_path, legacy_credential_dir, managed_agents_root, managed_memories_root,
    managed_skills_manifest_path, managed_skills_root, memories_archive_root, memory_backend_path,
    org_schema_path, resolve_memory_path, role_map_path, shared_agent_knowledge_dir,
    shared_prompt_path,
};

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
        if let Some(legacy) = legacy.as_ref() {
            tracing::warn!(
                path = %legacy,
                "memory path empty in config, falling back to legacy path"
            );
            return legacy.clone();
        }
        return default_path;
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
        Some(value) if value.eq_ignore_ascii_case("mem0") => "file".to_string(),
        Some(value) if value.eq_ignore_ascii_case("memento") => "memento".to_string(),
        Some(_) => DEFAULT_MEMORY_BACKEND.to_string(),
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
                // #3280: this loader sits on hot paths (turn finalize, periodic
                // ticks), and the json fallback is a supported config state — warn
                // once per process instead of flooding the logs on every lookup.
                static YAML_FALLBACK_WARN: std::sync::Once = std::sync::Once::new();
                if once_should_fire(&YAML_FALLBACK_WARN) {
                    tracing::warn!(
                        "memory backend config not found in YAML, falling back to memory-backend.json"
                    );
                }
                return config.normalized();
            }
        }
    }
    MemoryBackendConfig::default()
}

/// Returns `true` only for the first caller of the given [`std::sync::Once`]
/// (process-global once guard, same pattern as
/// `runtime_bootstrap::recover_orphan_pending_dispatches`). Used to bound a
/// hot-path log statement to a single emission per process (#3280).
fn once_should_fire(once: &std::sync::Once) -> bool {
    let mut first = false;
    once.call_once(|| first = true);
    first
}

pub fn shared_agent_knowledge_path(root: &Path) -> PathBuf {
    resolve_memory_path(root, &load_memory_backend(root).file.sak_path)
}

#[allow(dead_code)]
pub fn shared_agent_memory_root(root: &Path) -> PathBuf {
    resolve_memory_path(root, &load_memory_backend(root).file.sam_path)
}

pub fn long_term_memory_root(root: &Path) -> PathBuf {
    resolve_memory_path(root, &load_memory_backend(root).file.ltm_root)
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
    ensure_credential_layout(root)?;
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
    skill_sync::sync_managed_skills(root)
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
        credential_dir(root),
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

pub(crate) fn ensure_credential_layout(root: &Path) -> Result<(), String> {
    let canonical = credential_dir(root);
    let legacy = legacy_credential_dir(root);

    fs::create_dir_all(&canonical)
        .map_err(|e| format!("Failed to create '{}': {e}", canonical.display()))?;

    if path_exists(&legacy) {
        if same_canonical_path(&legacy, &canonical) {
            return Ok(());
        }

        migrate_legacy_credential_entries(&legacy, &canonical)?;
        remove_link_or_path(&legacy)?;
    }

    if !path_exists(&legacy) {
        if let Some(parent) = legacy.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create '{}': {e}", parent.display()))?;
        }
        create_symlink_entry(&canonical, &legacy, true)?;
    }

    Ok(())
}

fn migrate_legacy_credential_entries(legacy: &Path, canonical: &Path) -> Result<(), String> {
    for entry in read_dir_resolved(legacy)? {
        let Some(name) = entry.file_name() else {
            continue;
        };
        let destination = canonical.join(name);
        if path_exists(&destination) {
            continue;
        }
        copy_path_resolving_symlinks(&entry, &destination)?;
    }

    Ok(())
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
                tracing::warn!(
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

fn normalize_provider_name(value: &str) -> Option<String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "claude" => Some("claude".to_string()),
        "codex" => Some("codex".to_string()),
        "gemini" => Some("gemini".to_string()),
        "qwen" => Some("qwen".to_string()),
        _ => None,
    }
}

fn same_canonical_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(lhs), Ok(rhs)) => lhs == rhs,
        _ => a == b,
    }
}

fn create_symlink_entry(source: &Path, link_path: &Path, is_dir_link: bool) -> Result<(), String> {
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
        let _ = is_dir_link;
        use std::os::unix::fs::symlink;
        match symlink(&target, link_path) {
            Ok(()) => Ok(()),
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists
                    && same_canonical_path(link_path, source) =>
            {
                Ok(())
            }
            Err(error) => Err(format!(
                "Failed to create symlink '{}': {error}",
                link_path.display()
            )),
        }
    }
    #[cfg(windows)]
    {
        let result = if is_dir_link {
            std::os::windows::fs::symlink_dir(&target, link_path)
        } else {
            std::os::windows::fs::symlink_file(&target, link_path)
        };
        match result {
            Ok(()) => Ok(()),
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists
                    && same_canonical_path(link_path, source) =>
            {
                Ok(())
            }
            Err(error) => {
                let kind = if is_dir_link {
                    "dir symlink"
                } else {
                    "file symlink"
                };
                Err(format!(
                    "Failed to create {kind} '{}': {error}",
                    link_path.display()
                ))
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = is_dir_link;
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

fn remove_link_or_path(path: &Path) -> Result<(), String> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("Failed to stat '{}': {error}", path.display())),
    };
    if meta.file_type().is_symlink() {
        #[cfg(windows)]
        {
            if fs::metadata(path)
                .map(|target| target.is_dir())
                .unwrap_or(false)
            {
                return remove_dir_idempotent(path);
            }
        }
        return remove_file_idempotent(path);
    }

    if meta.is_dir() {
        remove_dir_all_idempotent(path)
    } else {
        #[cfg(windows)]
        {
            match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(file_err) if file_err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(file_err) if meta.file_type().is_symlink() => {
                    fs::remove_dir(path).or_else(|dir_err| {
                        if dir_err.kind() == std::io::ErrorKind::NotFound {
                            return Ok(());
                        }
                        Err(dir_err)
                    }).map_err(|dir_err| {
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
            remove_file_idempotent(path)
        }
    }
}

fn remove_file_idempotent(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Failed to remove '{}': {error}", path.display())),
    }
}

// Only reachable from the `#[cfg(windows)]` symlink-removal branch in
// `remove_link_or_path`; dead on other platforms.
#[cfg_attr(not(windows), allow(dead_code))]
fn remove_dir_idempotent(path: &Path) -> Result<(), String> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Failed to remove '{}': {error}", path.display())),
    }
}

fn remove_dir_all_idempotent(path: &Path) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Failed to remove '{}': {error}", path.display())),
    }
}

#[cfg(test)]
mod warn_once_tests {
    use super::*;

    #[test]
    fn once_should_fire_only_for_first_caller() {
        // #3280: the memory-backend YAML fallback WARN must fire exactly once
        // per process; test the guard with a local Once (the process-global
        // static at the call site shares this exact code path).
        let once = std::sync::Once::new();
        assert!(once_should_fire(&once));
        assert!(!once_should_fire(&once));
        assert!(!once_should_fire(&once));
    }

    #[test]
    fn once_should_fire_fires_exactly_once_across_threads() {
        let once = std::sync::Once::new();
        let fired = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    if once_should_fire(&once) {
                        fired.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                });
            }
        });
        assert_eq!(fired.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

#[cfg(test)]
mod credential_layout_tests {
    use super::*;

    #[test]
    fn ensure_credential_layout_creates_legacy_parent_for_fresh_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();

        assert!(!config_dir(root).exists());

        ensure_credential_layout(root).unwrap();

        assert!(credential_dir(root).is_dir());
        assert!(config_dir(root).is_dir());
        assert!(
            fs::symlink_metadata(legacy_credential_dir(root))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(same_canonical_path(
            &legacy_credential_dir(root),
            &credential_dir(root)
        ));
    }
}
