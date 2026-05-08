mod config_merge;
mod legacy_migration;
mod paths;
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use legacy_migration::remove_legacy_path;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use paths::{clear_test_home_dir_override, set_test_home_dir_override};
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use skill_sync::ensure_managed_skill_dir;

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
                tracing::warn!(
                    "memory backend config not found in YAML, falling back to memory-backend.json"
                );
                return config.normalized();
            }
        }
    }
    MemoryBackendConfig::default()
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
        symlink(&target, link_path)
            .map_err(|e| format!("Failed to create symlink '{}': {e}", link_path.display()))
    }
    #[cfg(windows)]
    {
        if is_dir_link {
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    struct TestLogWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for TestLogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs<T>(run: impl FnOnce() -> T) -> (T, String) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let log_buffer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .with_writer(move || TestLogWriter {
                buffer: log_buffer.clone(),
            })
            .finish();

        let result = tracing::subscriber::with_default(subscriber, run);
        let captured = buffer.lock().unwrap().clone();
        (result, String::from_utf8_lossy(&captured).to_string())
    }

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
        let migrated = role_map_path(root).with_extension("json.migrated");
        let role_map = fs::read_to_string(&migrated).unwrap();
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
    fn ensure_runtime_layout_migrates_legacy_credentials_into_canonical_dir() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();

        write_text(
            &legacy_credential_dir(root).join("announce_bot_token"),
            "announce-token\n",
        );
        write_text(
            &legacy_credential_dir(root).join("notify_bot_token"),
            "notify-token\n",
        );
        write_text(&credential_token_path(root, "claude"), "canonical-claude\n");
        write_text(
            &legacy_credential_dir(root).join("claude_bot_token"),
            "legacy-claude\n",
        );

        ensure_runtime_layout(root).unwrap();

        assert_eq!(
            fs::read_to_string(credential_token_path(root, "announce")).unwrap(),
            "announce-token\n"
        );
        assert_eq!(
            fs::read_to_string(credential_token_path(root, "notify")).unwrap(),
            "notify-token\n"
        );
        assert_eq!(
            fs::read_to_string(credential_token_path(root, "claude")).unwrap(),
            "canonical-claude\n"
        );
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
        assert_eq!(
            fs::read_to_string(legacy_credential_dir(root).join("announce_bot_token")).unwrap(),
            "announce-token\n"
        );
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
                    "max_participants": 6,
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
                            "prompt_file": "/tmp/config/role-context/project-agentdesk/IDENTITY.md",
                            "domain_summary": "Operations and delivery",
                            "strengths": ["planning", "coordination"],
                            "task_types": ["execution", "review"],
                            "anti_signals": ["legal"],
                            "provider_hint": "codex"
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
        assert_eq!(meeting.max_participants, Some(6));
        assert_eq!(meeting.available_agents.len(), 1);
        let available_agent = match &meeting.available_agents[0] {
            crate::config::MeetingAgentEntry::Detailed(agent) => agent,
            crate::config::MeetingAgentEntry::RoleId(_) => {
                panic!("expected detailed meeting agent")
            }
        };
        assert_eq!(available_agent.role_id, "project-agentdesk");
        assert_eq!(
            available_agent.domain_summary.as_deref(),
            Some("Operations and delivery")
        );
        assert_eq!(
            available_agent.strengths,
            vec!["planning".to_string(), "coordination".to_string()]
        );
        assert_eq!(
            available_agent.task_types,
            vec!["execution".to_string(), "review".to_string()]
        );
        assert_eq!(available_agent.anti_signals, vec!["legal".to_string()]);
        assert_eq!(available_agent.provider_hint.as_deref(), Some("codex"));

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
    fn role_map_merge_repairs_invalid_dispatch_profile() {
        let mut config: crate::config::Config = serde_yaml::from_str(
            r#"
server:
  port: 9001
agents:
  - id: project-agentdesk
    name: AgentDesk
    provider: codex
    channels:
      codex:
        id: "1479671301387059200"
        dispatch_profile: lte
"#,
        )
        .unwrap();
        let role_map = serde_json::json!({
            "byChannelId": {
                "1479671301387059200": {
                    "roleId": "project-agentdesk",
                    "provider": "codex",
                    "dispatchProfile": "lite"
                }
            }
        });

        assert!(preview_role_map_merge(&mut config, &role_map));
        let channel = config.agents[0].channels.codex.as_ref().unwrap();
        assert_eq!(channel.dispatch_profile().as_deref(), Some("lite"));
    }

    #[test]
    fn role_map_merge_drops_invalid_dispatch_profile_update() {
        let mut config: crate::config::Config = serde_yaml::from_str(
            r#"
server:
  port: 9001
agents: []
"#,
        )
        .unwrap();
        let role_map = serde_json::json!({
            "byChannelId": {
                "1479671301387059200": {
                    "roleId": "project-agentdesk",
                    "provider": "codex",
                    "dispatchProfile": "lte"
                }
            }
        });

        assert!(preview_role_map_merge(&mut config, &role_map));
        let channel = config.agents[0].channels.codex.as_ref().unwrap();
        assert_eq!(channel.dispatch_profile(), None);
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

        let (backend, logs) = capture_logs(|| load_memory_backend(root));

        assert_eq!(backend.version, 2);
        assert_eq!(backend.backend, "memento");
        assert_eq!(backend.file.sak_path, "/tmp/yaml/shared.md");
        assert_eq!(backend.file.sam_path, "/tmp/yaml/sam");
        assert_eq!(backend.file.ltm_root, "/tmp/yaml/ltm");
        assert_eq!(backend.file.auto_memory_root, "/tmp/yaml/auto/{workspace}");
        assert_eq!(backend.mcp.endpoint, "http://127.0.0.1:8765");
        assert_eq!(backend.mcp.access_key_env, "MEMENTO_API_KEY");
        assert!(logs.trim().is_empty());
    }

    #[test]
    fn load_memory_backend_treats_legacy_mem0_backend_as_file() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let _home_guard = TestHomeGuard::install(&temp.path().join("home"), root);
        write_text(
            &config_file_path(root),
            r#"server:
  port: 9001
memory:
  backend: mem0
"#,
        );

        let backend = load_memory_backend(root);

        assert_eq!(backend.backend, "file");
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

        let (backend, logs) = capture_logs(|| load_memory_backend(root));

        assert_eq!(backend.version, 1);
        assert_eq!(backend.backend, "auto");
        assert_eq!(backend.file.sak_path, "/tmp/legacy/shared.md");
        assert_eq!(backend.file.sam_path, "/tmp/legacy/sam");
        assert_eq!(backend.file.ltm_root, "/tmp/legacy/ltm");
        assert_eq!(
            backend.file.auto_memory_root,
            "~/.claude/projects/*{workspace}*/memory/"
        );
        assert!(logs.contains("memory backend config not found in YAML"));
    }

    #[test]
    fn normalize_file_memory_path_warns_when_empty_config_path_uses_legacy() {
        let legacy_path = "/tmp/legacy/shared.md".to_string();

        let (resolved, logs) = capture_logs(|| {
            normalize_file_memory_path(String::new(), Some(legacy_path.clone()), default_sak_path)
        });

        assert_eq!(resolved, legacy_path);
        assert!(logs.contains("memory path empty in config, falling back to legacy path"));
        assert!(logs.contains("/tmp/legacy/shared.md"));
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
