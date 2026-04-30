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
    load_shared_prompt_for_profile("full")
}

/// Profile-aware loader for the shared agent rules.
///
/// `_shared.prompt.md` may be partitioned with HTML-comment markers so that
/// review/headless dispatches strip out heavy "full" sections at load time:
///
/// ```text
/// <!-- profile: all -->          # always included (omit marker for same effect)
/// ...
/// <!-- /profile -->
/// <!-- profile: full -->         # only when profile == "full"
/// ...
/// <!-- /profile -->
/// <!-- profile: review-lite -->  # only when profile == "review-lite"
/// ...
/// <!-- /profile -->
/// <!-- profile: headless -->     # only when profile == "headless"
/// ...
/// <!-- /profile -->
/// ```
///
/// Files without any markers behave exactly like before (whole content kept).
pub(in crate::services::discord) fn load_shared_prompt_for_profile(
    profile: &str,
) -> Option<String> {
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
    let filtered = strip_non_matching_profile_sections(&raw, profile);
    const MAX_CHARS: usize = 6_000;
    if filtered.chars().count() <= MAX_CHARS {
        return Some(filtered);
    }
    let truncated: String = filtered.chars().take(MAX_CHARS).collect();
    Some(truncated)
}

/// Strip `<!-- profile: X -->` ... `<!-- /profile -->` blocks whose `X` does not
/// match `profile` (case-insensitive). Blocks tagged `all`, untagged content, and
/// matching blocks are preserved. Marker lines themselves are removed for clean
/// output. Unbalanced markers degrade gracefully — the whole section is kept.
fn strip_non_matching_profile_sections(raw: &str, profile: &str) -> String {
    let target = profile.trim().to_ascii_lowercase();
    let mut out = String::with_capacity(raw.len());
    let mut current_profile: Option<String> = None;

    for line in raw.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed
            .strip_prefix("<!-- profile:")
            .and_then(|s| s.strip_suffix("-->"))
        {
            current_profile = Some(rest.trim().to_ascii_lowercase());
            continue;
        }
        if trimmed == "<!-- /profile -->" {
            current_profile = None;
            continue;
        }
        let keep = match current_profile.as_deref() {
            None => true,
            Some("all") => true,
            Some(p) => p == target,
        };
        if keep {
            out.push_str(line);
            out.push('\n');
        }
    }

    // Collapse 3+ consecutive blank lines that profile stripping may produce.
    let mut compact = String::with_capacity(out.len());
    let mut blank_run = 0usize;
    for line in out.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                compact.push('\n');
            }
        } else {
            blank_run = 0;
            compact.push_str(line);
            compact.push('\n');
        }
    }
    compact.trim_end().to_string()
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod profile_tests {
    use super::strip_non_matching_profile_sections;

    const SAMPLE: &str = "head\n\
        <!-- profile: all -->\n\
        always\n\
        <!-- /profile -->\n\
        <!-- profile: full -->\n\
        only-full\n\
        <!-- /profile -->\n\
        <!-- profile: review-lite -->\n\
        only-review\n\
        <!-- /profile -->\n\
        <!-- profile: headless -->\n\
        only-headless\n\
        <!-- /profile -->\n\
        tail\n";

    #[test]
    fn full_profile_keeps_full_section() {
        let out = strip_non_matching_profile_sections(SAMPLE, "full");
        assert!(out.contains("only-full"));
        assert!(!out.contains("only-review"));
        assert!(!out.contains("only-headless"));
        assert!(out.contains("always"));
        assert!(out.contains("tail"));
    }

    #[test]
    fn review_lite_profile_strips_full_section() {
        let out = strip_non_matching_profile_sections(SAMPLE, "review-lite");
        assert!(!out.contains("only-full"));
        assert!(out.contains("only-review"));
        assert!(!out.contains("only-headless"));
        assert!(out.contains("always"));
    }

    #[test]
    fn headless_profile_strips_full_and_review() {
        let out = strip_non_matching_profile_sections(SAMPLE, "headless");
        assert!(!out.contains("only-full"));
        assert!(!out.contains("only-review"));
        assert!(out.contains("only-headless"));
        assert!(out.contains("always"));
    }

    #[test]
    fn unmarked_content_is_preserved_for_any_profile() {
        let raw = "## Code Principles\n- DRY\n";
        let out = strip_non_matching_profile_sections(raw, "review-lite");
        assert!(out.contains("DRY"));
    }

    #[test]
    fn marker_lines_are_stripped_from_output() {
        let out = strip_non_matching_profile_sections(SAMPLE, "full");
        assert!(!out.contains("<!-- profile:"));
        assert!(!out.contains("<!-- /profile -->"));
    }
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
        "Other specialist agents share this workspace. For requests mostly outside your scope:".to_string(),
        "1. Name 1-2 peer agents that fit better and why.".to_string(),
        "2. Ask \"해당 에이전트에게 전달할까요?\" and wait for approval.".to_string(),
        "3. On approval, call `agentdesk send-to-agent --from <self> --to <peer> --message \"...\" [--channel-kind cc|cdx]` to forward context via the announce bot so the peer intake_gate can trigger.".to_string(),
        "If the user wants your perspective anyway, answer within your scope and note the handoff option.".to_string(),
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
