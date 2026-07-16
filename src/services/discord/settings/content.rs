use super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Debug)]
struct RolePromptCacheEntry {
    modified: Option<SystemTime>,
    len: u64,
    content: String,
}

static ROLE_PROMPT_CACHE: OnceLock<Mutex<HashMap<PathBuf, RolePromptCacheEntry>>> = OnceLock::new();

pub(in crate::services::discord) fn load_role_prompt(binding: &RoleBinding) -> Option<String> {
    let prompt_path = Path::new(&binding.prompt_file);
    let (resolved_path, metadata) = resolve_role_prompt_path(prompt_path)?;
    let modified = metadata.modified().ok();
    let len = metadata.len();

    let cache = ROLE_PROMPT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = cache.lock()
        && let Some(entry) = guard.get(&resolved_path)
        && entry.modified == modified
        && entry.len == len
    {
        return Some(entry.content.clone());
    }

    let raw = fs::read_to_string(&resolved_path).ok()?;
    const MAX_CHARS: usize = 12_000;
    let content = if raw.chars().count() <= MAX_CHARS {
        raw
    } else {
        raw.chars().take(MAX_CHARS).collect()
    };
    if let Ok(mut guard) = cache.lock() {
        guard.insert(
            resolved_path,
            RolePromptCacheEntry {
                modified,
                len,
                content: content.clone(),
            },
        );
    }
    Some(content)
}

fn resolve_role_prompt_path(prompt_path: &Path) -> Option<(PathBuf, fs::Metadata)> {
    match fs::metadata(prompt_path) {
        Ok(metadata) if metadata.is_file() => Some((prompt_path.to_path_buf(), metadata)),
        _ => {
            let fallback = legacy_prompt_fallback_path(prompt_path)?;
            let metadata = fs::metadata(&fallback).ok()?;
            metadata.is_file().then_some((fallback, metadata))
        }
    }
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

#[cfg(test)]
mod role_prompt_cache_tests {
    use super::*;

    fn binding(path: &Path) -> RoleBinding {
        RoleBinding {
            role_id: "role-cache-test".to_string(),
            prompt_file: path.display().to_string(),
            provider: None,
            model: None,
            reasoning_effort: None,
            peer_agents_enabled: false,
            quality_feedback_injection_enabled: false,
            memory: ResolvedMemorySettings::default(),
        }
    }

    #[test]
    fn role_prompt_cache_invalidates_when_file_changes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("role.md");
        fs::write(&path, "v1").unwrap();

        assert_eq!(load_role_prompt(&binding(&path)).as_deref(), Some("v1"));
        std::thread::sleep(std::time::Duration::from_millis(5));
        fs::write(&path, "v2 with different len").unwrap();

        assert_eq!(
            load_role_prompt(&binding(&path)).as_deref(),
            Some("v2 with different len")
        );
    }
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
    let sections = matching_profile_sections(&raw, profile);
    const MAX_CHARS: usize = 12_000;
    let (filtered, truncated) = join_complete_sections_with_limit(&sections, MAX_CHARS);
    if truncated {
        tracing::warn!(
            target: "agentdesk.shared_prompt",
            path = %path_str,
            profile,
            max_chars = MAX_CHARS,
            kept_chars = filtered.chars().count(),
            "shared prompt exceeded character budget; truncated at a complete section boundary"
        );
    }
    if filtered.trim().is_empty() {
        return None;
    }
    Some(filtered)
}

#[derive(Debug)]
enum ProfileSectionNode {
    Text(String),
    Profile {
        name: String,
        open_marker: String,
        close_marker: Option<String>,
        children: Vec<ProfileSectionNode>,
    },
}

fn profile_open_marker(line: &str) -> Option<String> {
    line.trim()
        .strip_prefix("<!-- profile:")
        .and_then(|rest| rest.strip_suffix("-->"))
        .map(|profile| profile.trim().to_ascii_lowercase())
}

fn parse_profile_nodes(
    lines: &[&str],
    cursor: &mut usize,
    nested: bool,
) -> (Vec<ProfileSectionNode>, bool) {
    let mut nodes = Vec::new();
    while *cursor < lines.len() {
        let line = lines[*cursor];
        if let Some(name) = profile_open_marker(line) {
            let open_marker = format!("{line}\n");
            *cursor += 1;
            let (children, closed) = parse_profile_nodes(lines, cursor, true);
            nodes.push(ProfileSectionNode::Profile {
                name,
                open_marker,
                close_marker: closed.then(|| "<!-- /profile -->\n".to_string()),
                children,
            });
            continue;
        }
        if line.trim() == "<!-- /profile -->" {
            *cursor += 1;
            if nested {
                return (nodes, true);
            }
            continue;
        }

        let mut text = String::new();
        while *cursor < lines.len()
            && profile_open_marker(lines[*cursor]).is_none()
            && lines[*cursor].trim() != "<!-- /profile -->"
        {
            text.push_str(lines[*cursor]);
            text.push('\n');
            *cursor += 1;
        }
        if !text.is_empty() {
            nodes.push(ProfileSectionNode::Text(text));
        }
    }
    (nodes, false)
}

fn render_profile_nodes(
    nodes: &[ProfileSectionNode],
    target: &str,
    parent_matches: bool,
    force_keep: bool,
) -> String {
    let mut rendered = String::new();
    for node in nodes {
        match node {
            ProfileSectionNode::Text(text) => {
                if parent_matches || force_keep {
                    rendered.push_str(text);
                }
            }
            ProfileSectionNode::Profile {
                name,
                open_marker,
                close_marker,
                children,
            } => {
                if force_keep || close_marker.is_none() {
                    rendered.push_str(open_marker);
                    rendered.push_str(&render_profile_nodes(children, target, true, true));
                    if let Some(close_marker) = close_marker {
                        rendered.push_str(close_marker);
                    }
                } else {
                    let section_matches = parent_matches && (name == "all" || name == target);
                    rendered.push_str(&render_profile_nodes(
                        children,
                        target,
                        section_matches,
                        false,
                    ));
                }
            }
        }
    }
    rendered
}

fn matching_profile_sections(raw: &str, profile: &str) -> Vec<String> {
    let lines: Vec<_> = raw.lines().collect();
    let mut cursor = 0;
    let (nodes, _) = parse_profile_nodes(&lines, &mut cursor, false);
    let target = profile.trim().to_ascii_lowercase();
    nodes
        .iter()
        .map(|node| render_profile_nodes(std::slice::from_ref(node), &target, true, false))
        .filter(|section| !section.trim().is_empty())
        .collect()
}

fn compact_blank_lines(raw: &str) -> String {
    let mut compact = String::with_capacity(raw.len());
    let mut previous_blank = false;
    for line in raw.lines() {
        let blank = line.trim().is_empty();
        if !blank || !previous_blank {
            compact.push_str(line);
            compact.push('\n');
        }
        previous_blank = blank;
    }
    compact.trim_end().to_string()
}

fn join_complete_sections_with_limit(sections: &[String], max_chars: usize) -> (String, bool) {
    let mut accepted = String::new();
    let mut rendered = String::new();
    for section in sections {
        let mut candidate = accepted.clone();
        candidate.push_str(section);
        let compact = compact_blank_lines(&candidate);
        if compact.chars().count() > max_chars {
            return (rendered, true);
        }
        accepted = candidate;
        rendered = compact;
    }
    (rendered, false)
}

/// Strip `<!-- profile: X -->` ... `<!-- /profile -->` blocks whose `X` does not
/// match `profile` (case-insensitive). Blocks tagged `all`, untagged content, and
/// matching blocks are preserved. Marker lines for balanced sections are removed
/// for clean output. Unbalanced sections keep their marker lines so malformed input
/// is not silently discarded.
#[cfg(test)]
fn strip_non_matching_profile_sections(raw: &str, profile: &str) -> String {
    let sections = matching_profile_sections(raw, profile);
    compact_blank_lines(&sections.concat())
}

#[cfg(test)]
mod shared_prompt_profile_tests {
    use super::*;

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
    fn profiles_keep_only_all_unmarked_and_matching_sections() {
        let full = strip_non_matching_profile_sections(SAMPLE, "full");
        assert!(full.contains("always"));
        assert!(full.contains("only-full"));
        assert!(!full.contains("only-review"));
        assert!(!full.contains("only-headless"));
        assert!(full.contains("head"));
        assert!(full.contains("tail"));
        assert!(!full.contains("<!-- profile:"));

        let review = strip_non_matching_profile_sections(SAMPLE, "review-lite");
        assert!(review.contains("always"));
        assert!(review.contains("only-review"));
        assert!(!review.contains("only-full"));

        let headless = strip_non_matching_profile_sections(SAMPLE, "headless");
        assert!(headless.contains("always"));
        assert!(headless.contains("only-headless"));
        assert!(!headless.contains("only-full"));
    }

    #[test]
    fn unclosed_nonmatching_section_preserves_its_content() {
        let raw = "before\n<!-- profile: full -->\nUNFINISHED FULL\ntail\n";
        let out = strip_non_matching_profile_sections(raw, "headless");
        assert_eq!(out, "before\n<!-- profile: full -->\nUNFINISHED FULL\ntail");
    }

    #[test]
    fn nested_close_restores_outer_profile_state() {
        let raw = "<!-- profile: full -->\nouter-start\n\
                   <!-- profile: headless -->\ninner-headless\n<!-- /profile -->\n\
                   OUTER FULL TAIL\n<!-- /profile -->\n";
        let headless = strip_non_matching_profile_sections(raw, "headless");
        assert!(!headless.contains("outer-start"));
        assert!(!headless.contains("inner-headless"));
        assert!(!headless.contains("OUTER FULL TAIL"));
        let full = strip_non_matching_profile_sections(raw, "full");
        assert!(full.contains("outer-start"));
        assert!(!full.contains("inner-headless"));
        assert!(full.contains("OUTER FULL TAIL"));
    }

    #[test]
    fn unclosed_outer_section_preserves_nested_nonmatching_content() {
        let raw = "<!-- profile: full -->\nouter-start\n\
                   <!-- profile: headless -->\ninner-headless\n<!-- /profile -->\n\
                   UNFINISHED OUTER TAIL\n";
        let review = strip_non_matching_profile_sections(raw, "review-lite");
        assert!(review.contains("<!-- profile: full -->"));
        assert!(review.contains("outer-start"));
        assert!(review.contains("<!-- profile: headless -->"));
        assert!(review.contains("inner-headless"));
        assert!(review.contains("<!-- /profile -->"));
        assert!(review.contains("UNFINISHED OUTER TAIL"));
    }

    #[test]
    fn compaction_collapses_two_or_more_blank_lines_to_one() {
        assert_eq!(compact_blank_lines("alpha\n\n\n\nbeta\n"), "alpha\n\nbeta");
    }

    #[test]
    fn character_limit_keeps_only_complete_profile_sections() {
        let first = format!(
            "<!-- profile: all -->\n{}\nFIRST SECTION END\n<!-- /profile -->\n",
            "a".repeat(40)
        );
        let second = format!(
            "<!-- profile: full -->\nSECOND SECTION START\n{}\nSECOND SECTION END\n<!-- /profile -->\n",
            "b".repeat(80)
        );
        let sections = matching_profile_sections(&(first + &second), "full");
        let first_only = compact_blank_lines(&sections[0]);
        let (out, truncated) =
            join_complete_sections_with_limit(&sections, first_only.chars().count() + 1);

        assert!(truncated);
        assert_eq!(out, first_only);
        assert!(out.contains("FIRST SECTION END"));
        assert!(!out.contains("SECOND SECTION START"));
        assert!(!out.contains("SECOND SECTION END"));
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

/// #2663: process-local cache for the rendered `[Peer Agent Directory]`
/// block. The peer agent directory is ~1.5KB and rebuilt every turn even
/// though the underlying config files (`agentdesk.yaml`, `org_schema`,
/// `role_map`) change once per deploy at most. The cache key is
/// `(current_role_id, mtime_fingerprint)`; whenever any input file's mtime
/// changes (or a file disappears) the fingerprint shifts and the next call
/// re-renders.
fn peer_guidance_cache() -> &'static std::sync::Mutex<PeerGuidanceCacheState> {
    static CELL: std::sync::OnceLock<std::sync::Mutex<PeerGuidanceCacheState>> =
        std::sync::OnceLock::new();
    CELL.get_or_init(|| std::sync::Mutex::new(PeerGuidanceCacheState::default()))
}

#[derive(Default)]
struct PeerGuidanceCacheState {
    fingerprint: Option<PeerSourceFingerprint>,
    /// (role_id → cached rendering). `None` means "no peers for this role".
    entries: std::collections::HashMap<String, Option<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
struct PeerSourceFingerprint {
    /// `Option<SystemTime>` per source path; `None` means the file does not
    /// exist (deletion is also a cache-busting event).
    parts: Vec<(std::path::PathBuf, Option<std::time::SystemTime>)>,
}

fn peer_source_paths() -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    if let Some(root) = crate::config::runtime_root() {
        paths.push(crate::runtime_layout::config_file_path(&root));
        paths.push(crate::runtime_layout::legacy_config_file_path(&root));
        paths.push(crate::runtime_layout::role_map_path(&root));
        paths.push(crate::runtime_layout::org_schema_path(&root));
    }
    paths
}

fn current_peer_source_fingerprint() -> PeerSourceFingerprint {
    let parts = peer_source_paths()
        .into_iter()
        .map(|path| {
            let mtime = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .ok();
            (path, mtime)
        })
        .collect();
    PeerSourceFingerprint { parts }
}

/// #2663: test-only helper to drop the peer guidance cache between scenarios.
#[cfg(test)]
#[allow(dead_code)] // #3034: test cache-reset helper; no active test caller currently.
pub(in crate::services::discord) fn invalidate_peer_guidance_cache_for_tests() {
    let mut guard = peer_guidance_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = PeerGuidanceCacheState::default();
}

pub(in crate::services::discord) fn render_peer_agent_guidance(
    current_role_id: &str,
) -> Option<String> {
    // #2663: fast path — same role, unchanged source files → reuse cached
    // rendering (a String clone is much cheaper than the load+filter+format
    // pipeline below).
    let fingerprint = current_peer_source_fingerprint();
    {
        let guard = peer_guidance_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.fingerprint.as_ref() == Some(&fingerprint) {
            if let Some(entry) = guard.entries.get(current_role_id) {
                return entry.clone();
            }
        }
    }

    let peers: Vec<PeerAgentInfo> = load_peer_agents()
        .into_iter()
        .filter(|agent| agent.role_id != current_role_id)
        .collect();
    if peers.is_empty() {
        let mut guard = peer_guidance_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.fingerprint.as_ref() != Some(&fingerprint) {
            guard.fingerprint = Some(fingerprint);
            guard.entries.clear();
        }
        guard.entries.insert(current_role_id.to_string(), None);
        return None;
    }

    let mut lines = vec![
        "[Peer Agent Directory]".to_string(),
        "Other specialist agents share this workspace. For requests mostly outside your scope:".to_string(),
        "1. Name 1-2 peer agents that fit better and why.".to_string(),
        "2. Ask \"해당 에이전트에게 전달할까요?\" and wait for approval.".to_string(),
        "3. On approval, call `agentdesk send-to-agent --from <self> --to <peer> --message \"...\" --expect-reply <true|false> [--channel-kind cc|cdx]` to forward context via the announce bot so the peer intake_gate can trigger. Use `true` when a reply is needed, otherwise `false`.".to_string(),
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

    let rendered = lines.join("\n");
    // #2663: store the freshly-rendered block under the current fingerprint.
    // If the fingerprint changed since the fast-path check, we reset the
    // role-keyed map so we never serve a rendering produced against an
    // out-of-date source file.
    let mut guard = peer_guidance_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.fingerprint.as_ref() != Some(&fingerprint) {
        guard.fingerprint = Some(fingerprint);
        guard.entries.clear();
    }
    guard
        .entries
        .insert(current_role_id.to_string(), Some(rendered.clone()));
    Some(rendered)
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
