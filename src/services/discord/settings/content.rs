use super::*;

pub(in crate::services::discord) fn load_role_prompt(binding: &RoleBinding) -> Option<String> {
    let prompt_path = Path::new(&binding.prompt_file);
    let raw = fs::read_to_string(prompt_path)
        .or_else(|_| {
            legacy_prompt_fallback_path(prompt_path)
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
                .and_then(fs::read_to_string)
        })
        .ok()?;
    const MAX_CHARS: usize = 12_000;
    if raw.chars().count() <= MAX_CHARS {
        return Some(raw);
    }
    let truncated: String = raw.chars().take(MAX_CHARS).collect();
    Some(truncated)
}

pub(super) fn legacy_prompt_fallback_path(path: &Path) -> Option<PathBuf> {
    let mut rewritten = PathBuf::new();
    let mut replaced = false;

    for component in path.components() {
        match component {
            Component::Normal(name) if name == "role-context" => {
                rewritten.push("agents");
                replaced = true;
            }
            other => rewritten.push(other.as_os_str()),
        }
    }

    replaced.then_some(rewritten)
}

pub(crate) fn load_longterm_memory_catalog(role_id: &str) -> Option<String> {
    let memory_dir = runtime_store::long_term_memory_root()?.join(role_id);
    if !memory_dir.is_dir() {
        let root = runtime_store::agentdesk_root()?;
        let legacy_dir = root
            .join("role-context")
            .join(format!("{}.memory", role_id));
        if !legacy_dir.is_dir() {
            return None;
        }
        return load_longterm_memory_catalog_from_dir(&legacy_dir);
    }
    load_longterm_memory_catalog_from_dir(&memory_dir)
}

pub(super) fn load_longterm_memory_catalog_from_dir(
    memory_dir: &std::path::Path,
) -> Option<String> {
    let mut entries: Vec<(String, String)> = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(memory_dir) else {
        return None;
    };

    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().map_or(true, |ext| ext != "md") {
            continue;
        }
        let filename = path.file_name()?.to_string_lossy().to_string();
        let content = std::fs::read_to_string(&path).unwrap_or_default();

        let description = extract_frontmatter_description(&content)
            .or_else(|| extract_first_heading(&content))
            .unwrap_or_else(|| filename.trim_end_matches(".md").to_string());

        let abs_path = path.display().to_string();
        entries.push((abs_path, description));
    }

    if entries.is_empty() {
        return None;
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let catalog: Vec<String> = entries
        .iter()
        .map(|(path, desc)| format!("  - {}: {}", path, desc))
        .collect();

    Some(catalog.join("\n"))
}

pub(super) fn extract_frontmatter_description(content: &str) -> Option<String> {
    if !content.starts_with("---") {
        return None;
    }
    let rest = &content[3..];
    let end = rest.find("\n---")?;
    let frontmatter = &rest[..end];
    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if let Some(desc) = trimmed.strip_prefix("description:") {
            let desc = desc.trim().trim_matches('"').trim_matches('\'');
            if !desc.is_empty() {
                return Some(desc.to_string());
            }
        }
    }
    None
}

pub(super) fn extract_first_heading(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix('#') {
            let heading = heading.trim_start_matches('#').trim();
            if !heading.is_empty() {
                return Some(heading.to_string());
            }
        }
    }
    None
}

pub(in crate::services::discord) fn load_shared_prompt() -> Option<String> {
    let path_str = agentdesk_config::load_shared_prompt_path()
        .or_else(|| {
            if org_schema::org_schema_exists() {
                org_schema::load_shared_prompt_path()
            } else {
                None
            }
        })
        .or_else(load_shared_prompt_path_from_role_map)?;

    let raw = fs::read_to_string(Path::new(&path_str)).ok()?;
    const MAX_CHARS: usize = 6_000;
    if raw.chars().count() <= MAX_CHARS {
        return Some(raw);
    }
    let truncated: String = raw.chars().take(MAX_CHARS).collect();
    Some(truncated)
}

pub(in crate::services::discord) fn load_review_tuning_guidance() -> Option<String> {
    let root = runtime_store::agentdesk_root()?;
    let path = root.join("runtime").join("review-tuning-guidance.txt");
    let content = fs::read_to_string(path).ok()?;
    if content.trim().is_empty() {
        return None;
    }
    const MAX_CHARS: usize = 2_000;
    if content.chars().count() <= MAX_CHARS {
        Some(content)
    } else {
        Some(content.chars().take(MAX_CHARS).collect())
    }
}

pub(in crate::services::discord) fn is_known_agent(role_id: &str) -> bool {
    if let Some(known) = agentdesk_config::is_known_agent(role_id) {
        return known;
    }
    if org_schema::org_schema_exists()
        && let Some(known) = org_schema::is_known_agent(role_id)
    {
        return known;
    }
    is_known_agent_from_role_map(role_id)
}

pub(super) fn load_peer_agents() -> Vec<PeerAgentInfo> {
    let peers = agentdesk_config::load_peer_agents();
    if !peers.is_empty() {
        return peers;
    }
    if org_schema::org_schema_exists() {
        let peers = org_schema::load_peer_agents();
        if !peers.is_empty() {
            return peers;
        }
    }
    load_peer_agents_from_role_map()
}

pub(in crate::services::discord) fn render_peer_agent_guidance(
    current_role_id: &str,
) -> Option<String> {
    let peers: Vec<PeerAgentInfo> = load_peer_agents()
        .into_iter()
        .filter(|agent| agent.role_id != current_role_id)
        .collect();
    if peers.is_empty() {
        return None;
    }

    let mut lines = vec![
        "[Peer Agent Directory]".to_string(),
        "You are one role agent among multiple specialist agents in this workspace.".to_string(),
        "If a request is mostly outside your scope, do not bluff ownership or silently proceed as if it were yours.".to_string(),
        "Instead, name the 1-2 most suitable peer agents below, explain why they fit better, and ask: \"해당 에이전트에게 전달할까요?\"".to_string(),
        "If the user approves, use the `send-agent-message` skill to forward the request context to the recommended agent.".to_string(),
        "If the user explicitly wants your perspective anyway, answer only within your scope and mention the handoff option.".to_string(),
        String::new(),
        "Available peer agents:".to_string(),
    ];

    for peer in peers {
        let keywords = if peer.keywords.is_empty() {
            String::new()
        } else {
            let short = peer.keywords.iter().take(4).cloned().collect::<Vec<_>>();
            format!(" — best for: {}", short.join(", "))
        };
        lines.push(format!(
            "- {} ({}){}",
            peer.role_id, peer.display_name, keywords
        ));
    }

    Some(lines.join("\n"))
}

pub(in crate::services::discord) fn channel_upload_dir(
    channel_id: ChannelId,
) -> Option<std::path::PathBuf> {
    discord_uploads_root().map(|p| p.join(channel_id.get().to_string()))
}

pub(in crate::services::discord) fn cleanup_old_uploads(max_age: Duration) {
    let Some(root) = discord_uploads_root() else {
        return;
    };
    if !root.exists() {
        return;
    }

    let now = SystemTime::now();
    let Ok(channels) = fs::read_dir(&root) else {
        return;
    };

    for ch in channels.filter_map(|e| e.ok()) {
        let ch_path = ch.path();
        if !ch_path.is_dir() {
            continue;
        }

        let Ok(files) = fs::read_dir(&ch_path) else {
            continue;
        };

        for f in files.filter_map(|e| e.ok()) {
            let f_path = f.path();
            if !f_path.is_file() {
                continue;
            }

            let should_delete = fs::metadata(&f_path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|mtime| now.duration_since(mtime).ok())
                .map(|age| age >= max_age)
                .unwrap_or(false);

            if should_delete {
                let _ = fs::remove_file(&f_path);
            }
        }

        if fs::read_dir(&ch_path)
            .ok()
            .map(|mut it| it.next().is_none())
            .unwrap_or(false)
        {
            let _ = fs::remove_dir(&ch_path);
        }
    }
}

pub(in crate::services::discord) fn cleanup_channel_uploads(channel_id: ChannelId) {
    if let Some(dir) = channel_upload_dir(channel_id) {
        let _ = fs::remove_dir_all(dir);
    }
}
