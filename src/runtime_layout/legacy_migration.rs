use super::paths::{
    default_long_term_memory_root, default_shared_agent_knowledge_path,
    default_shared_agent_memory_root,
};
use super::*;

pub(super) fn create_legacy_backup(root: &Path) -> Result<PathBuf, String> {
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

pub(super) fn migrate_legacy_layout(root: &Path) -> Result<(), String> {
    migrate_legacy_config_file(root)?;
    migrate_memory_backend_file(root)?;
    migrate_role_context(root)?;
    migrate_shared_agent_memory(root)?;
    Ok(())
}

pub(super) fn normalize_agent_config_channels(root: &Path) -> Result<(), String> {
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

pub(super) fn synchronize_shared_prompt(root: &Path) -> Result<(), String> {
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
        if let Err(error) = create_symlink_entry(&canonical, &alias, false) {
            if path_exists(&alias) {
                remove_link_or_path(&alias)?;
                create_symlink_entry(&canonical, &alias, false)?;
            } else {
                return Err(error);
            }
        }
    }

    Ok(())
}

fn shared_prompt_aliases(root: &Path) -> Vec<PathBuf> {
    let mut aliases = vec![
        config_dir(root).join("_shared.md"),
        managed_agents_root(root).join("_shared.md"),
    ];
    if let Some(home) = super::paths::current_home_dir() {
        aliases.push(home.join(".agentdesk").join("prompts").join("_shared.md"));
    }
    aliases
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

pub(super) fn remove_legacy_path(path: &Path) -> Result<(), String> {
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
