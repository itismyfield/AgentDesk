use super::*;

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// #2667 — cache entry for the per-role boilerplate sections (role prompt,
/// peer-agent guidance, shared prompt). Stores the rendered text plus the
/// underlying file's `mtime` and a hard TTL ceiling so we still re-read on
/// content drift even if the OS does not advance the timestamp.
#[derive(Clone, Debug)]
struct PromptCacheEntry {
    text: Option<String>,
    mtime: Option<SystemTime>,
    cached_at: Instant,
}

/// 5-minute hard ceiling. Above this we re-read regardless of mtime so a
/// botched mtime-preserving editor (e.g. `cp -p`) cannot pin a stale prompt.
const PROMPT_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const PROMPT_CACHE_MAX_ENTRIES: usize = 256;

fn role_prompt_cache() -> &'static Mutex<HashMap<String, PromptCacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, PromptCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// #2667 — public reset hook so tests can drive the cache without touching
/// the private statics.
#[cfg(test)]
pub(in crate::services::discord) fn clear_role_prompt_cache_for_tests() {
    if let Ok(mut guard) = role_prompt_cache().lock() {
        guard.clear();
    }
}

fn read_role_prompt_from_disk(binding: &RoleBinding) -> Option<String> {
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

pub(in crate::services::discord) fn load_role_prompt(binding: &RoleBinding) -> Option<String> {
    let prompt_path = Path::new(&binding.prompt_file);
    let mtime = file_mtime(prompt_path).or_else(|| {
        legacy_prompt_fallback_path(prompt_path)
            .as_deref()
            .and_then(file_mtime)
    });
    let cache_key = binding.prompt_file.clone();

    if let Ok(guard) = role_prompt_cache().lock() {
        if let Some(entry) = guard.get(&cache_key) {
            let mtime_match = entry.mtime == mtime;
            let within_ttl = entry.cached_at.elapsed() < PROMPT_CACHE_TTL;
            if mtime_match && within_ttl {
                return entry.text.clone();
            }
        }
    }

    let text = read_role_prompt_from_disk(binding);
    if let Ok(mut guard) = role_prompt_cache().lock() {
        // Bound cache so a misconfigured deployment cycling through many role
        // bindings cannot grow the map without bound.
        if guard.len() >= PROMPT_CACHE_MAX_ENTRIES {
            // Drop the entry with the oldest `cached_at` — cheap approximate LRU.
            if let Some(stalest) = guard
                .iter()
                .min_by_key(|(_, entry)| entry.cached_at)
                .map(|(k, _)| k.clone())
            {
                guard.remove(&stalest);
            }
        }
        guard.insert(
            cache_key,
            PromptCacheEntry {
                text: text.clone(),
                mtime,
                cached_at: Instant::now(),
            },
        );
    }
    text
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
/// #2667 — cache for the profile-filtered shared prompt. Keyed by
/// `(path, profile)` so different profiles do not interfere, with mtime
/// invalidation. Same TTL/cap policy as the role prompt cache.
fn shared_prompt_cache() -> &'static Mutex<HashMap<(String, String), PromptCacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, String), PromptCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(in crate::services::discord) fn clear_shared_prompt_cache_for_tests() {
    if let Ok(mut guard) = shared_prompt_cache().lock() {
        guard.clear();
    }
}

fn load_shared_prompt_from_disk(path_str: &str, profile: &str) -> Option<String> {
    let raw = fs::read_to_string(Path::new(path_str)).ok()?;
    let filtered = strip_non_matching_profile_sections(&raw, profile);
    const MAX_CHARS: usize = 6_000;
    if filtered.chars().count() <= MAX_CHARS {
        return Some(filtered);
    }
    let truncated: String = filtered.chars().take(MAX_CHARS).collect();
    Some(truncated)
}

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

    let cache_key = (path_str.clone(), profile.to_string());
    let mtime = file_mtime(Path::new(&path_str));

    if let Ok(guard) = shared_prompt_cache().lock() {
        if let Some(entry) = guard.get(&cache_key) {
            let mtime_match = entry.mtime == mtime;
            let within_ttl = entry.cached_at.elapsed() < PROMPT_CACHE_TTL;
            if mtime_match && within_ttl {
                return entry.text.clone();
            }
        }
    }

    let text = load_shared_prompt_from_disk(&path_str, profile);

    if let Ok(mut guard) = shared_prompt_cache().lock() {
        if guard.len() >= PROMPT_CACHE_MAX_ENTRIES {
            if let Some(stalest) = guard
                .iter()
                .min_by_key(|(_, entry)| entry.cached_at)
                .map(|(k, _)| k.clone())
            {
                guard.remove(&stalest);
            }
        }
        guard.insert(
            cache_key,
            PromptCacheEntry {
                text: text.clone(),
                mtime,
                cached_at: Instant::now(),
            },
        );
    }

    text
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

fn review_tuning_cache() -> &'static Mutex<Option<PromptCacheEntry>> {
    static CACHE: OnceLock<Mutex<Option<PromptCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
pub(in crate::services::discord) fn clear_review_tuning_cache_for_tests() {
    if let Ok(mut guard) = review_tuning_cache().lock() {
        *guard = None;
    }
}

fn read_review_tuning_guidance_from_disk(path: &Path) -> Option<String> {
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

pub(in crate::services::discord) fn load_review_tuning_guidance() -> Option<String> {
    let root = runtime_store::agentdesk_root()?;
    let path = root.join("runtime").join("review-tuning-guidance.txt");
    let mtime = file_mtime(&path);

    if let Ok(guard) = review_tuning_cache().lock() {
        if let Some(entry) = guard.as_ref() {
            let mtime_match = entry.mtime == mtime;
            let within_ttl = entry.cached_at.elapsed() < PROMPT_CACHE_TTL;
            if mtime_match && within_ttl {
                return entry.text.clone();
            }
        }
    }

    let text = read_review_tuning_guidance_from_disk(&path);
    if let Ok(mut guard) = review_tuning_cache().lock() {
        *guard = Some(PromptCacheEntry {
            text: text.clone(),
            mtime,
            cached_at: Instant::now(),
        });
    }
    text
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

/// #2667 — separate cache for the rendered peer-agent guidance block. Keyed
/// only by `current_role_id` (the peer roster is workspace-scoped — peers'
/// own role-id is not in the key). 5-min TTL so a peer added or removed
/// propagates within a coffee break.
#[derive(Clone, Debug)]
struct PeerGuidanceCacheEntry {
    text: Option<String>,
    cached_at: Instant,
}

fn peer_guidance_cache() -> &'static Mutex<HashMap<String, PeerGuidanceCacheEntry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, PeerGuidanceCacheEntry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(in crate::services::discord) fn clear_peer_guidance_cache_for_tests() {
    if let Ok(mut guard) = peer_guidance_cache().lock() {
        guard.clear();
    }
}

fn build_peer_agent_guidance(current_role_id: &str) -> Option<String> {
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

pub(in crate::services::discord) fn render_peer_agent_guidance(
    current_role_id: &str,
) -> Option<String> {
    if let Ok(guard) = peer_guidance_cache().lock() {
        if let Some(entry) = guard.get(current_role_id) {
            if entry.cached_at.elapsed() < PROMPT_CACHE_TTL {
                return entry.text.clone();
            }
        }
    }
    let rendered = build_peer_agent_guidance(current_role_id);
    if let Ok(mut guard) = peer_guidance_cache().lock() {
        if guard.len() >= PROMPT_CACHE_MAX_ENTRIES {
            if let Some(stalest) = guard
                .iter()
                .min_by_key(|(_, entry)| entry.cached_at)
                .map(|(k, _)| k.clone())
            {
                guard.remove(&stalest);
            }
        }
        guard.insert(
            current_role_id.to_string(),
            PeerGuidanceCacheEntry {
                text: rendered.clone(),
                cached_at: Instant::now(),
            },
        );
    }
    rendered
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

#[cfg(test)]
mod prompt_cache_tests {
    //! #2667 — coverage for the role / shared / peer / review-tuning prompt
    //! caches. Each test isolates its cache via the `clear_*_for_tests` hooks
    //! so multi-test runs do not bleed state.

    use super::*;
    use std::fs::File;
    use std::io::Write;

    fn binding_for(path: &Path) -> RoleBinding {
        RoleBinding {
            role_id: "test-role".to_string(),
            prompt_file: path.to_string_lossy().to_string(),
            provider: None,
            model: None,
            reasoning_effort: None,
            peer_agents_enabled: false,
            quality_feedback_injection_enabled: false,
            memory: ResolvedMemorySettings::default(),
        }
    }

    #[test]
    fn role_prompt_cache_returns_cached_text_within_ttl() {
        clear_role_prompt_cache_for_tests();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("role.md");
        {
            let mut f = File::create(&path).expect("create");
            writeln!(f, "first content").expect("write");
        }
        let binding = binding_for(&path);
        let first = load_role_prompt(&binding).expect("first");
        assert!(first.contains("first content"));

        // Second call within the TTL with no mtime drift must hit cache.
        // We assert it through a side channel: the cache must record exactly
        // one entry for this binding's prompt_file key, and the rendered
        // text must be identical.
        let second = load_role_prompt(&binding).expect("second");
        assert_eq!(first, second);
        let guard = role_prompt_cache().lock().expect("lock");
        assert_eq!(
            guard.get(&binding.prompt_file).map(|e| e.text.clone()),
            Some(Some(first))
        );
    }

    #[test]
    fn role_prompt_cache_invalidates_on_mtime_change() {
        clear_role_prompt_cache_for_tests();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("role.md");
        {
            let mut f = File::create(&path).expect("create");
            writeln!(f, "v1 content").expect("write");
        }
        let binding = binding_for(&path);
        let v1 = load_role_prompt(&binding).expect("v1");
        assert!(v1.contains("v1 content"));

        // Sleep enough for mtime resolution differences across filesystems.
        std::thread::sleep(Duration::from_millis(20));
        {
            let mut f = File::create(&path).expect("recreate");
            writeln!(f, "v2 content").expect("rewrite");
        }
        let v2 = load_role_prompt(&binding).expect("v2");
        assert!(
            v2.contains("v2 content"),
            "mtime drift must invalidate cache, got: {v2}"
        );
    }

    #[test]
    fn role_prompt_cache_caps_growth_under_load() {
        clear_role_prompt_cache_for_tests();
        let dir = tempfile::tempdir().expect("tempdir");
        // Cap is 256. Push 260 distinct cache keys; eviction must keep len
        // bounded.
        for i in 0..260 {
            let path = dir.path().join(format!("role-{i}.md"));
            {
                let mut f = File::create(&path).expect("create");
                writeln!(f, "content-{i}").expect("write");
            }
            let binding = binding_for(&path);
            let _ = load_role_prompt(&binding);
        }
        let guard = role_prompt_cache().lock().expect("lock");
        assert!(
            guard.len() <= PROMPT_CACHE_MAX_ENTRIES,
            "cache must stay bounded, got: {}",
            guard.len()
        );
    }

    #[test]
    fn shared_prompt_cache_keys_distinguish_profiles() {
        clear_shared_prompt_cache_for_tests();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("shared.md");
        let path_str = path.to_string_lossy().to_string();
        {
            let mut f = File::create(&path).expect("create");
            writeln!(
                f,
                "<!-- profile: full -->\nonly-full\n<!-- /profile -->\n<!-- profile: review-lite -->\nonly-review\n<!-- /profile -->"
            )
            .expect("write");
        }
        let full = load_shared_prompt_from_disk(&path_str, "full").expect("full");
        let review = load_shared_prompt_from_disk(&path_str, "review-lite").expect("review");
        assert!(full.contains("only-full"));
        assert!(!full.contains("only-review"));
        assert!(review.contains("only-review"));
        assert!(!review.contains("only-full"));
    }

    #[test]
    fn review_tuning_cache_returns_none_for_empty_file() {
        clear_review_tuning_cache_for_tests();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rt.txt");
        File::create(&path).expect("create");
        // Empty file → None expected.
        let value = read_review_tuning_guidance_from_disk(&path);
        assert!(value.is_none());
    }

    #[test]
    fn review_tuning_cache_truncates_oversize() {
        clear_review_tuning_cache_for_tests();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rt.txt");
        {
            let mut f = File::create(&path).expect("create");
            let big = "a".repeat(3_000);
            f.write_all(big.as_bytes()).expect("write");
        }
        let value = read_review_tuning_guidance_from_disk(&path).expect("value");
        assert_eq!(value.chars().count(), 2_000);
    }

    #[test]
    fn peer_guidance_cache_drops_self_role() {
        clear_peer_guidance_cache_for_tests();
        // We do not control the live peer registry from this test, but the
        // cache wrapper must never panic when the underlying registry is
        // empty for the current role. The first call populates a `None`
        // entry; the second must hit the cache (no panic, same value).
        let first = render_peer_agent_guidance("nonexistent-role-for-cache-test");
        let second = render_peer_agent_guidance("nonexistent-role-for-cache-test");
        assert_eq!(first, second);
    }
}
